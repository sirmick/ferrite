//! End-to-end Morse round-trip: text → `MorseAudioSource` → `MorseDemod`
//! → decoded text. No SDR, no off-air recording, no fixtures — pure
//! synthesis through the same multimon code path the live `cw.json`
//! preset uses.
//!
//! Asserts that the decoded log lines, concatenated and uppercased,
//! contain the source text. Multimon's CW decoder may insert leading
//! spaces, mis-decode the very first character before its bit timing
//! locks, or break a line at silence — `contains` is the right shape
//! for that. We send several repetitions of a short word so the
//! decoder has plenty of bursts to lock against.

#![cfg(feature = "multimon")]

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    Block, MorseAudioSource, MorseAudioSourceParams, MorseDemod, MorseDemodParams,
};

const RATE_HZ: f32 = 22_050.0;

fn generate_morse_audio(text: &str, wpm: u32, n_samples: usize) -> Vec<f32> {
    let mut src = MorseAudioSource::new(MorseAudioSourceParams {
        rate_hz: RATE_HZ,
        text: text.to_string(),
        wpm,
        tone_hz: 800.0,
        amplitude: 0.7,
        loop_sequence: true,
    })
    .expect("morse source");
    let mut out = vec![0.0_f32; n_samples];
    let mut outputs = [OutputPort {
        name: "out",
        meta: PortMeta::default(),
        buf: OutBuf::RealF32(&mut out),
    }];
    let mut io = BlockIo {
        inputs: &mut [],
        outputs: &mut outputs,
    };
    src.process(&mut io).unwrap();
    out
}

fn decode_morse(audio: &[f32]) -> Vec<String> {
    let mut demod = MorseDemod::new(MorseDemodParams {
        sample_rate_hz: RATE_HZ,
    })
    .expect("morse demod");
    // Pump in 4096-sample chunks — matches the wrapper's batch size and
    // exercises the same code path the live runtime hits.
    let mut lines = Vec::new();
    let mut consumed = 0;
    while consumed < audio.len() {
        let take = (audio.len() - consumed).min(4096);
        let chunk = &audio[consumed..consumed + take];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(chunk),
        }];
        let mut outputs: [OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        demod.process(&mut io).unwrap();
        consumed += take;
        // Multimon writes through tracing rather than a return value, so
        // we capture by subscribing — but tracing isn't easy to attach
        // here. Instead, the underlying MultimonDemod's per-thread buffer
        // is drained inside MorseDemod::process(). We can't read it from
        // outside; rely on tracing capture via a custom Layer.
        let _ = lines.is_empty(); // placeholder — see custom subscriber below.
    }
    lines
}

/// Capture every `decoder::cw` info line into a Vec while a closure
/// runs. Simpler than wiring a tracing-test layer: install a default
/// subscriber for the duration via `tracing::subscriber::with_default`
/// and route writes through a shared `Mutex<Vec<String>>`.
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
    let writer = VecWriter(Arc::clone(&buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_target(true)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);

    let bytes = buf.lock().unwrap();
    let s = String::from_utf8_lossy(&bytes);
    s.lines()
        .filter(|l| l.contains("decoder::cw"))
        .map(str::to_string)
        .collect()
}

#[test]
fn round_trip_text_decodes_back() {
    // Generate ~15 s of audio so multimon has plenty of repetitions to
    // lock its bit timing. PARIS at 18 wpm = ~3.3 s per repetition.
    let n = (RATE_HZ as usize) * 15;
    let audio = generate_morse_audio("PARIS PARIS PARIS PARIS", 18, n);

    let captured = with_cw_capture(|| {
        decode_morse(&audio);
    });

    // Each captured line looks like ` INFO decoder::cw: P` — pull out
    // the payload after the last `:` and concatenate. Multimon emits
    // one character per line.
    let payload: String = captured
        .iter()
        .filter_map(|line| line.rsplit_once(':').map(|(_, p)| p.trim().to_string()))
        .collect::<Vec<_>>()
        .join("")
        .to_uppercase();
    assert!(
        payload.contains("PARIS"),
        "expected PARIS in decoded payload {payload:?}, raw lines: {captured:?}",
    );
}
