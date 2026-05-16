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
//! `psk31, olivia-8-500, contestia-8-500, throb4` decode straight off
//! the raw recording (their wide capture / AFC pulls them in). The
//! narrow-FSK / tuning-sensitive ones need the modem pointed at the
//! signal: headless has no waterfall click and the shim's
//! `powerDensity` stub returns 0, so the FSK/RTTY AFC sig-search is
//! inert. With the `rtty_reverse` + `rx_freq_hz` knobs now on
//! `FldigiDemod`, `rtty45` (rev=false, 1000 Hz), `mt63-1000L`
//! (1500 Hz) and `dominoex16` (rev=true, 1220 Hz) also decode — see
//! `CASES`. Only `navtex` (SITOR-B, time-diversity FEC) still won't
//! sync off this clip and stays `#[ignore]`d. `probe_*` print decodes
//! for diagnosis and never fail.

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

/// `(mode id, 8 kHz wav stem, rtty_reverse, rx_freq_hz)` — the tuned
/// params each mode needs to decode the real clip. `0.0` = leave
/// fldigi's default carrier (AFC-pulled modes). Empirically found by
/// the `probe_*` sweeps.
struct Case {
    mode: &'static str,
    base: &'static str,
    reverse: bool,
    rx_freq_hz: f32,
}
const fn c(mode: &'static str, base: &'static str, reverse: bool, rx_freq_hz: f32) -> Case {
    Case {
        mode,
        base,
        reverse,
        rx_freq_hz,
    }
}
const CASES: &[Case] = &[
    c("rtty45", "RTTY_170Hz_45.45bd", false, 1000.0),
    c("psk31", "BPSK31", false, 0.0),
    c("olivia-8-500", "Olivia_8-500", false, 0.0),
    c("contestia-8-500", "Contestia_8-500", false, 0.0),
    c("mt63-1000L", "MT63-1000L", false, 1500.0),
    c("dominoex16", "DominoEX_16Bd", true, 1220.0),
    c("throb4", "THROB4", false, 0.0),
    c("navtex", "NAVTEX_SITOR-B", false, 0.0),
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

/// Decode one recording with explicit RTTY polarity + RX carrier
/// (`0.0` = fldigi default); return the reassembled `decoder::fldigi`
/// text.
fn decode_full(mode: &str, base: &str, rtty_reverse: bool, rx_freq_hz: f32) -> String {
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
        rtty_reverse,
        rx_freq_hz,
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

fn case(mode: &str) -> &'static Case {
    CASES.iter().find(|c| c.mode == mode).expect("known mode")
}

/// Shared gate: every clip is the pangram, so a correct decode contains
/// "QUICK BROWN FOX" regardless of case or per-mode lock-in garbage.
fn assert_decodes_pangram(mode: &str) {
    let k = case(mode);
    let got = decode_full(k.mode, k.base, k.reverse, k.rx_freq_hz);
    assert!(
        got.to_uppercase().contains("QUICK BROWN FOX"),
        "{mode} (rev={}, rx_freq={}): real recording did not decode the \
         pangram (got {got:?})",
        k.reverse,
        k.rx_freq_hz,
    );
}

#[test]
fn rtty45_real_recording() {
    assert_decodes_pangram("rtty45");
}

#[test]
fn psk31_real_recording() {
    assert_decodes_pangram("psk31");
}

#[test]
fn olivia_8_500_real_recording() {
    assert_decodes_pangram("olivia-8-500");
}

#[test]
fn contestia_8_500_real_recording() {
    assert_decodes_pangram("contestia-8-500");
}

#[test]
fn throb4_real_recording() {
    assert_decodes_pangram("throb4");
}

#[test]
fn mt63_1000l_real_recording() {
    assert_decodes_pangram("mt63-1000L");
}

#[test]
fn dominoex16_real_recording() {
    assert_decodes_pangram("dominoex16");
}

#[test]
#[ignore = "real NAVTEX/SITOR-B clip won't FEC-sync off this recording \
             at any swept carrier/polarity (time-diversity FEC + weak, \
             spread tone energy ~2080 Hz in the clip). Re-enable with a \
             cleaner SITOR-B fixture. probe_navtex_sweep shows the \
             attempts."]
fn navtex_real_recording() {
    assert_decodes_pangram("navtex");
}

/// Diagnostic (always passes, run with `--nocapture`): the decode each
/// gate mode produces with its tuned `CASES` params.
#[test]
fn probe_all_real_fldigi_modes() {
    println!("\n--- real sigidwiki fldigi decode (tuned params) ---");
    for k in CASES {
        let text = decode_full(k.mode, k.base, k.reverse, k.rx_freq_hz);
        let printable: String = text.chars().filter(|c| !c.is_control()).collect();
        println!(
            "{:>16} rev={} f={:>6} | {}",
            k.mode,
            k.reverse,
            k.rx_freq_hz,
            printable.chars().take(72).collect::<String>()
        );
    }
    println!("--- end ---\n");
}

/// Diagnostic for the still-failing NAVTEX clip: sweep carrier ×
/// polarity so a future cleaner fixture / tuning can be dialed in.
#[test]
fn probe_navtex_sweep() {
    for f in [0.0_f32, 1000.0, 1500.0, 2000.0, 2080.0, 2100.0] {
        for rev in [false, true] {
            let t = decode_full("navtex", "NAVTEX_SITOR-B", rev, f);
            let p: String = t.chars().filter(|c| !c.is_control()).collect();
            if p.trim().len() >= 4 {
                println!(
                    "navtex rev={rev} f={f:>6}: {:?}",
                    p.chars().take(64).collect::<String>()
                );
            }
        }
    }
}
