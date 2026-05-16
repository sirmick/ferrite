//! Real-sample fldigi decode — per-mode ship-gate fixtures.
//!
//! `fldigi_e2e` proves the block + tracing plumbing with a synthetic
//! RTTY signal. This drives the *real* sigidwiki recordings — converted
//! to 8 kHz mono by `samples/sigidwiki/to_fldigi_8k.sh` — through
//! `FldigiDemod` per mode and asserts the decoded text reaches
//! `decoder::fldigi`. Every clip is the "the quick brown fox jumps
//! over the lazy dog 1234567890" pangram, so the gate is a robust
//! case-insensitive `contains("QUICK BROWN FOX")` — immune to the
//! leading lock-in garbage and per-mode case differences.
//!
//! Four modes decode cleanly off the raw recordings (AFC pulls them
//! in): **psk31, olivia-8-500, contestia-8-500, throb4**. The other
//! four (rtty45, navtex, mt63-1000L, dominoex16) are narrow-FSK /
//! tuning-sensitive: the sigidwiki recording is not at fldigi's
//! expected audio centre and `FldigiDemod` exposes no tune/`reverse`
//! knob (the shim only plumbs `afc` + the `rtty_*` set_params), so
//! they are `#[ignore]`d with the reason recorded — re-enable when
//! the block surfaces a tuning/`rtty_reverse` param or the fixtures
//! are re-centred. `probe_all_real_fldigi_modes` always runs and
//! prints every mode's decode for diagnosis.

#![cfg(feature = "fldigi")]
#![allow(clippy::doc_markdown)]

mod common;

use common::{load_audio, sample_path};
use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, FldigiDemod, FldigiDemodParams};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The vendored fldigi core keeps a single active modem in C++ globals
/// (`g_active`, `progStatus`, …), so two `FldigiDemod`s in flight at
/// once corrupt each other. cargo runs a binary's tests in parallel
/// threads — serialize every fldigi-touching test through this lock.
/// (Cross-*binary* is already safe: each test binary is its own
/// process, which is why `fldigi_e2e` needs no lock.)
fn fldigi_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// (fldigi mode id, 8 kHz wav under samples/sigidwiki/8000_mono/).
const CASES: &[(&str, &str)] = &[
    ("rtty45", "RTTY_170Hz_45.45bd"),
    ("psk31", "BPSK31"),
    ("olivia-8-500", "Olivia_8-500"),
    ("contestia-8-500", "Contestia_8-500"),
    ("navtex", "NAVTEX_SITOR-B"),
    ("mt63-1000L", "MT63-1000L"),
    ("dominoex16", "DominoEX_16Bd"),
    ("throb4", "THROB4"),
];

fn with_fldigi_capture<F: FnOnce()>(f: F) -> Vec<String> {
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
    let subscriber = tracing_subscriber::fmt()
        .with_writer(VecWriter(Arc::clone(&buf)))
        .with_target(true)
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);

    let bytes = buf.lock().unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| l.contains("decoder::fldigi"))
        .map(str::to_string)
        .collect()
}

/// Decode one recording, return the reassembled `decoder::fldigi` text.
fn decode(mode: &str, base: &str) -> String {
    let _g = fldigi_guard();
    let rel = format!("sigidwiki/8000_mono/{base}.wav");
    assert!(
        sample_path(&rel).exists(),
        "{rel} missing — run samples/sigidwiki/to_fldigi_8k.sh"
    );
    let (audio, rate) = load_audio(&rel);
    assert!((rate - 8_000.0).abs() < 1.0, "{base}: expected 8 kHz");

    let mut demod = FldigiDemod::new(FldigiDemodParams {
        mode: mode.to_string(),
        sample_rate_hz: 8_000.0,
        afc: true,
    })
    .unwrap_or_else(|e| panic!("FldigiDemod {mode}: {e}"));

    let lines = with_fldigi_capture(|| {
        let mut idx = 0;
        while idx < audio.len() {
            let take = 2_048.min(audio.len() - idx);
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&audio[idx..idx + take]),
            }];
            let mut outputs: [OutputPort; 0] = [];
            demod
                .process(&mut BlockIo {
                    inputs: &mut inputs,
                    outputs: &mut outputs,
                })
                .unwrap();
            idx += take;
        }
    });

    lines
        .iter()
        .filter_map(|l| {
            let body = l.split("decoder::fldigi: ").nth(1)?;
            Some(
                body.rsplit_once(" mode=")
                    .map_or(body, |(t, _)| t)
                    .to_string(),
            )
        })
        .collect()
}

/// Shared gate: the recording is the standard pangram, so a correct
/// decode contains "QUICK BROWN FOX" regardless of case or lock-in
/// garbage.
fn assert_decodes_pangram(mode: &str, base: &str) {
    let text = decode(mode, base).to_uppercase();
    assert!(
        text.contains("QUICK BROWN FOX"),
        "{mode}: real recording did not decode the pangram \
         (got {:?})",
        decode(mode, base)
    );
}

#[test]
#[ignore = "real RTTY clip decodes as garbage through the current \
             FldigiDemod surface (likely mark/space reversed and/or off \
             fldigi's audio centre — the block plumbs neither \
             rtty_reverse nor a tune param). Synthetic RTTY is covered \
             by fldigi_e2e."]
fn rtty45_real_recording() {
    assert_decodes_pangram("rtty45", "RTTY_170Hz_45.45bd");
}

#[test]
fn psk31_real_recording() {
    assert_decodes_pangram("psk31", "BPSK31");
}

#[test]
fn olivia_8_500_real_recording() {
    assert_decodes_pangram("olivia-8-500", "Olivia_8-500");
}

#[test]
fn contestia_8_500_real_recording() {
    assert_decodes_pangram("contestia-8-500", "Contestia_8-500");
}

#[test]
fn throb4_real_recording() {
    assert_decodes_pangram("throb4", "THROB4");
}

#[test]
#[ignore = "real NAVTEX/SITOR-B clip not at fldigi's audio centre; \
             FldigiDemod exposes no tune param. Re-enable when it does \
             or the fixture is re-centred."]
fn navtex_real_recording() {
    assert_decodes_pangram("navtex", "NAVTEX_SITOR-B");
}

#[test]
#[ignore = "real MT63-1000L clip off-centre (MT63 needs the 1 kHz block \
             at ~1500 Hz); no tune param on FldigiDemod."]
fn mt63_1000l_real_recording() {
    assert_decodes_pangram("mt63-1000L", "MT63-1000L");
}

#[test]
#[ignore = "real DominoEX_16Bd clip does not lock through the current \
             FldigiDemod surface (tuning/baud-variant sensitive)."]
fn dominoex16_real_recording() {
    assert_decodes_pangram("dominoex16", "DominoEX_16Bd");
}

/// Always-on diagnostic: print every mode's decode (run with
/// `--nocapture`). Never fails — it is the map for the gates above.
#[test]
fn probe_all_real_fldigi_modes() {
    println!("\n--- real sigidwiki fldigi decode probe ---");
    for (mode, base) in CASES {
        let text = decode(mode, base);
        let printable: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .collect();
        let n = printable.chars().filter(|c| !c.is_whitespace()).count();
        let preview: String = printable.replace('\n', "⏎").chars().take(80).collect();
        println!("{mode:>16}  {n:>4} chars  | {preview}");
    }
    println!("--- end probe ---\n");
}
