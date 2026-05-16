//! End-to-end Morse decode of the real sigidwiki CW recording.
//!
//! `morse_e2e` round-trips synthetic tone-keyed audio. This drives the
//! actual off-air capture `samples/sigidwiki/22050_mono/Cw_morse.wav`
//! (22 050 Hz mono s16, derived from the upstream MP3 by `convert.py`)
//! through `MorseDemod` and asserts decoded text reaches the
//! `decoder::cw` tracing target — the same transport the UI consumes.
//! Real off-air CW has hand-sent timing jitter, QSB and noise the
//! synthetic test never exercises.

#![cfg(feature = "multimon")]

mod common;

use common::{load_audio, sample_path};
use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, MorseDemod, MorseDemodParams};

const RATE_HZ: f32 = 22_050.0;

fn with_cw_capture<F: FnOnce()>(f: F) -> Vec<String> {
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
        .filter(|l| l.contains("decoder::cw"))
        .map(str::to_string)
        .collect()
}

#[test]
fn real_cw_recording_decodes_text() {
    let rel = "sigidwiki/22050_mono/Cw_morse.wav";
    assert!(
        sample_path(rel).exists(),
        "{rel} missing — run samples/sigidwiki/convert.py"
    );
    let (audio, rate) = load_audio(rel);
    assert!((rate - f64::from(RATE_HZ)).abs() < 1.0, "expected 22050 Hz");
    // Full RX flowgraph: CW is a keyed carrier copied as an SSB tone.
    let audio = common::full_chain_ssb(&audio, f64::from(RATE_HZ));

    let mut demod = MorseDemod::new(MorseDemodParams {
        sample_rate_hz: RATE_HZ,
    })
    .expect("morse demod");

    let lines = with_cw_capture(|| {
        let mut idx = 0;
        while idx < audio.len() {
            let take = 4_096.min(audio.len() - idx);
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

    // Reassemble the per-letter `decoder::cw` stream.
    let decoded: String = lines
        .iter()
        .filter_map(|l| l.split("decoder::cw: ").nth(1).map(str::to_string))
        .collect::<String>();
    let letters: String = decoded.chars().filter(|c| !c.is_whitespace()).collect();
    println!(
        "decoded {} cw lines, {} letters: {decoded:?}",
        lines.len(),
        letters.len()
    );

    // Hand-sent off-air CW: don't pin exact text (timing jitter / QSB
    // mangle some chars), but a working chain must produce a real
    // run of decoded letters, not silence or a stray glitch char.
    assert!(
        letters.len() >= 8,
        "expected a substantial CW decode from the real recording, got {letters:?}"
    );
}
