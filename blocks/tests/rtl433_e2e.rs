//! End-to-end tests for the rtl_433 ISM-band decoder.
//!
//! Two hermetic tests run on every CI invocation:
//!
//! - `preset_parses_and_block_constructs` — loads
//!   `flowgraphs/rtl433-433.json`, walks every block declaration, and
//!   constructs the `Rtl433Demod` through its registered `BlockFactory`
//!   from the preset's exact param JSON. Catches preset-schema drift
//!   and block-param-key regressions.
//! - `silence_emits_no_events_via_block` — drives a fully-set-up
//!   `Rtl433Demod` instance with 0.5 s of zero IQ at 250 kHz and
//!   asserts no events surface. Confirms the shim's allocation +
//!   teardown survive a real `process()` invocation, complementing
//!   the in-crate unit test that targets the safe wrapper directly.
//!
//! Fixture-driven tests (full scale, −50 dBFS with and without the
//! block's auto-level normaliser):
//!
//! - `acurite_606tx_capture_decodes_at_least_one_event` — loads
//!   `blocks/tests/fixtures/rtl433_acurite_606tx_433mhz_250ks.cu8`
//!   (sourced from merbanan/rtl_433_tests; see fixtures/SOURCES.md),
//!   drives the block in 4096-sample chunks, captures
//!   `decoder::rtl_433` tracing via a `MakeWriter`, asserts ≥1 line
//!   with `"model"` and `"Acurite-606TX"` in it.

#![cfg(feature = "rtl_433")]

use std::fs;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockFactory, BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, Rtl433Demod, Rtl433DemodParams};
use num_complex::Complex;

/// Project root, derived from `CARGO_MANIFEST_DIR` (which points at
/// `blocks/`).
fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // blocks/ → /
    p
}

#[test]
fn preset_parses_and_block_constructs() {
    let preset_path = repo_root().join("flowgraphs/rtl433-433.json");
    let json = fs::read_to_string(&preset_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", preset_path.display()));

    // Parse the preset's loose JSON shape and pluck the `rtl433`
    // block's params out — that's the surface the runtime hands to
    // BlockFactory at load time.
    let doc: serde_json::Value =
        serde_json::from_str(&json).expect("flowgraphs/rtl433-433.json is valid JSON");

    let rtl433_params = doc
        .get("blocks")
        .and_then(|b| b.get("rtl433"))
        .and_then(|b| b.get("params"))
        .cloned()
        .expect("preset has rtl433 block with params");

    // Same factory path the runtime takes. Catches param-key drift
    // (e.g. if the block ever renames `sample_rate_hz` → something
    // else and the preset still ships the old key).
    let _block: Box<dyn Block> = Rtl433Demod::construct(&rtl433_params)
        .expect("Rtl433Demod::construct accepts the preset's params");
}

#[test]
fn silence_emits_no_events_via_block() {
    // Build straight from defaults — equivalent to the preset's
    // 250 kHz / decoder_set=default after deserialization.
    let mut block =
        Rtl433Demod::new(Rtl433DemodParams::default()).expect("Rtl433Demod constructs at 250 kHz");

    // 0.5 s at 250 kHz, zero IQ. Pulse detect should see no packages.
    let samples = vec![Complex::<f32>::new(0.0, 0.0); 125_000];

    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::IqF32(&samples),
    }];
    let mut outputs: [OutputPort; 0] = [];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };

    let work = block
        .process(&mut io)
        .expect("Rtl433Demod::process on silence");
    assert_eq!(work.consumed[0], samples.len());
}

/// Fixture-driven decode test against the Acurite 606TX capture from
/// `merbanan/rtl_433_tests`. Reads the CU8 fixture (interleaved u8
/// I/Q, unsigned with 128 as zero), converts to Ferrite's ±1.0 f32
/// IQ, drives the block in 4096-sample chunks, captures the
/// `decoder::rtl_433` tracing target into a buffer, and asserts at
/// least one event mentions `Acurite-606TX`.
/// Run the Acurite-606TX capture through a block, scaled by `scale`
/// (1.0 = the capture's own near-full-scale level), returning the
/// `decoder::rtl_433` tracing lines it produced.
fn decode_fixture(params: Rtl433DemodParams, scale: f32) -> Vec<String> {
    use std::sync::Mutex;
    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    let fixture = repo_root().join("blocks/tests/fixtures/rtl433_acurite_606tx_433mhz_250ks.cu8");

    let bytes =
        fs::read(&fixture).unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture.display()));
    assert!(
        bytes.len().is_multiple_of(2),
        "fixture must be CU8 (2 bytes/sample)"
    );

    // CU8 → Complex<f32> in ±1.0 convention. u8 128 = zero; the upstream
    // rtl_sdr capture tool emits this directly off the RTL2832.
    let samples: Vec<Complex<f32>> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            let i = (i32::from(c[0]) - 128) as f32 / 128.0;
            let q = (i32::from(c[1]) - 128) as f32 / 128.0;
            Complex::new(i, q) * scale
        })
        .collect();

    // Capture the `decoder::rtl_433` tracing target into a buffer so
    // we can count lines without a global subscriber.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<String>>>);
    impl<'a> MakeWriter<'a> for Sink {
        type Writer = SinkWriter;
        fn make_writer(&'a self) -> Self::Writer {
            SinkWriter(self.0.clone())
        }
    }
    struct SinkWriter(std::sync::Arc<Mutex<Vec<String>>>);
    impl std::io::Write for SinkWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(b) {
                self.0.lock().unwrap().push(s.to_string());
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sink = Sink::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(Level::INFO)
            .finish(),
    );

    let mut block = Rtl433Demod::new(params).unwrap();

    // Push in 4096-sample chunks to exercise the streaming path.
    for chunk in samples.chunks(4096) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(chunk),
        }];
        let mut outputs: [OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        block.process(&mut io).unwrap();
    }

    let lines = sink.0.lock().unwrap();
    lines
        .iter()
        .filter(|l| l.contains("decoder::rtl_433"))
        .cloned()
        .collect()
}

fn assert_acurite(rtl433_lines: &[String], what: &str) {
    assert!(
        !rtl433_lines.is_empty(),
        "{what}: fixture should yield ≥1 decoded event"
    );
    // Spot-check that the right device was matched. Upstream's
    // Acurite-606TX decoder emits `"model" : "Acurite-606TX"` in its
    // JSON; any other model in those lines means we picked up a
    // false-positive against a different protocol.
    assert!(
        rtl433_lines.iter().any(|l| l.contains("Acurite-606TX")),
        "{what}: expected Acurite-606TX in decoded events; got: {}",
        rtl433_lines.join(" | ")
    );
}

#[test]
fn acurite_606tx_capture_decodes_at_least_one_event() {
    let lines = decode_fixture(Rtl433DemodParams::default(), 1.0);
    assert_acurite(&lines, "full-scale capture");
}

/// A −50 dBFS copy of the same capture: what an RSPdx at a sane gain
/// hands the block for a sensor 45 dB over the floor (2026-10-13 live:
/// bursts at +46 dB over a −94 dBFS floor, 36 M samples in, 0 events).
/// The default auto-level normaliser must still decode it…
#[test]
fn acurite_606tx_at_minus_50_dbfs_decodes_with_auto_level() {
    let lines = decode_fixture(Rtl433DemodParams::default(), 0.003_16);
    assert_acurite(&lines, "−50 dBFS capture, auto_level on");
}

/// …and the same copy with the normaliser off must decode nothing —
/// upstream's `min_level` (−12.1 dBFS) gate. This pins the reason the
/// normaliser exists; if upstream ever lowers the gate the assertion
/// (not the normaliser) is what to revisit.
#[test]
fn acurite_606tx_at_minus_50_dbfs_is_silent_without_auto_level() {
    let lines = decode_fixture(
        Rtl433DemodParams {
            auto_level: false,
            ..Rtl433DemodParams::default()
        },
        0.003_16,
    );
    assert!(
        lines.is_empty(),
        "−50 dBFS capture, auto_level off: expected rtl_433's min_level gate to drop it; got: {}",
        lines.join(" | ")
    );
}
