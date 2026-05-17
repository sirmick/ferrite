//! End-to-end ADS-B round-trip against an upstream test recording.
//!
//! Loads `blocks/tests/fixtures/modes1.bin` — the BSD-licensed test
//! file shipped with antirez's classic dump1090 (see that directory's
//! `SOURCES.md`) — feeds the raw u8 I/Q samples through `AdsbDemod`
//! and asserts that the known-good frames inside (DF 17 ADS-B from
//! ICAO `4d2023`, plus DF 11 / DF 4 surveillance replies) decode.
//!
//! The sigidwiki ADS-B page links its own IQ recording on mega.nz
//! only; that's enough friction that we ship the dump1090 upstream
//! fixture instead. The catalog entry (`flowgraphs/adsb.json`) still
//! carries the sigidwiki link via `signal_wiki_url` so users have a
//! pointer to canonical reference material.

#![cfg(feature = "adsb")]

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::aircraft_spot::AircraftSpot;
use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{AdsbDemod, AdsbDemodParams, Block};
use ferrite_dump1090::Dump1090;
use num_complex::Complex;

/// Read the dump1090 reference IQ — u8 interleaved, 127 = zero, 2 MS/s.
fn read_modes1_iq() -> Vec<Complex<f32>> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "modes1.bin",
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .expect("open fixtures/modes1.bin — pruned?")
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
        .chunks_exact(2)
        .map(|c| {
            Complex::new(
                (f32::from(c[0]) - 128.0) / 128.0,
                (f32::from(c[1]) - 128.0) / 128.0,
            )
        })
        .collect()
}

/// Capture every `decoder::adsb` info line emitted while `f` runs.
/// Same technique as `ais_e2e::with_ais_capture` and
/// `morse_e2e::with_cw_capture`.
fn with_adsb_capture<F: FnOnce()>(f: F) -> Vec<String> {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = VecWriter(Arc::clone(&buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_target(true)
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);

    let bytes = buf.lock().unwrap();
    let s = String::from_utf8_lossy(&bytes);
    s.lines()
        .filter(|l| l.contains("decoder::adsb"))
        .map(str::to_string)
        .collect()
}

#[test]
fn modes1_decodes_known_frames() {
    let iq = read_modes1_iq();
    assert!(!iq.is_empty(), "fixture had no IQ samples");

    let mut block = AdsbDemod::new(AdsbDemodParams::default()).expect("adsb demod");

    // Push in 800-sample chunks (≈400 µs at 2 MS/s) — matches the
    // live runtime tick rate so the wrapper's batching loop sees the
    // same chunking pattern. The internal MODES_DATA_LEN buffer
    // (~256 KB) absorbs whatever caller chunk size we use.
    let chunk = 800;
    let lines = with_adsb_capture(|| {
        let mut idx = 0;
        while idx < iq.len() {
            let take = chunk.min(iq.len() - idx);
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::IqF32(&iq[idx..idx + take]),
            }];
            let mut outputs: [OutputPort; 0] = [];
            let mut io = BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            };
            block.process(&mut io).unwrap();
            idx += take;
        }
    });

    let joined = lines.join("\n");
    // Three independent assertions — each names a frame the upstream
    // dump1090 binary also reports against this fixture, so any of
    // these failing means our wrap broke a known-good decode path.
    assert!(
        joined.contains("DF 17"),
        "expected a DF 17 ADS-B frame; saw {} lines:\n{}",
        lines.len(),
        joined
    );
    assert!(
        joined.contains("4d2023"),
        "expected ICAO 4d2023 (the DF 17 sender in modes1.bin) in the decoded output"
    );
    assert!(joined.contains("DF 11"), "expected a DF 11 all-call reply");
}

/// Ship-gate for the `ui:adsb` advanced view: the real modes1.bin
/// decode, run through `Dump1090::aircraft_snapshot` →
/// `AircraftSpot::write_json` (the exact path `AdsbDemod`'s events
/// port uses), must produce store-contract JSON (see
/// `web/src/lib/adsb/store.svelte.ts`). Drives `Dump1090` directly:
/// the fixture is ~0.18 s, below the block's 1 Hz snapshot throttle,
/// same reason ft8/wspr e2e bypass their blocks' wall-clock gates.
#[test]
fn adsb_events_emit_store_contract_json() {
    let iq = read_modes1_iq();
    let mut d = Dump1090::new();
    for c in iq.chunks(800) {
        d.push_iq(c);
    }
    let _ = d.drain_lines(); // text path exercised by the test above
    let acs = d.aircraft_snapshot();
    assert!(
        !acs.is_empty(),
        "modes1.bin should yield ≥1 tracked aircraft"
    );

    let mut buf = Vec::new();
    for a in &acs {
        AircraftSpot {
            icao: a.icao,
            flight: &a.flight,
            pos: a.position,
            alt_ft: a.altitude_ft,
            gs_kt: a.speed_kt,
            trk_deg: a.track_deg,
            msgs: a.messages,
            age_s: a.age_s,
        }
        .write_json(&mut buf);
    }

    let text = String::from_utf8(buf).expect("events bytes must be UTF-8");
    let mut rows = 0;
    let mut saw_4d2023 = false;
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid aircraft JSON {line:?}: {e}"));
        // Fields the store reads unconditionally.
        assert!(v["icao"].is_string(), "icao missing in {line:?}");
        assert!(v["alt"].is_number(), "alt missing in {line:?}");
        assert!(v["gs"].is_number(), "gs missing in {line:?}");
        assert!(v["trk"].is_number(), "trk missing in {line:?}");
        assert!(v["msgs"].is_number(), "msgs missing in {line:?}");
        assert!(v["age"].is_number(), "age missing in {line:?}");
        // lat/lon, when present, must be a coherent pair.
        assert_eq!(
            v.get("lat").is_some(),
            v.get("lon").is_some(),
            "lat/lon must appear together in {line:?}"
        );
        if v["icao"] == "4D2023" {
            saw_4d2023 = true;
        }
        rows += 1;
    }
    assert_eq!(rows, acs.len(), "one JSON row per tracked aircraft");
    // 4d2023 is the DF 17 sender the text-path test also asserts —
    // serialized upper-hex here.
    assert!(
        saw_4d2023,
        "expected ICAO 4D2023 in the snapshot; got:\n{text}"
    );
    eprintln!("ADS-B contract OK: {rows} aircraft");
}
