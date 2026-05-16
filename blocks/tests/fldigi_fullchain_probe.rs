//! Probe: fldigi decode through the *real* RX flowgraph.
//!
//! `FileAudioSource → SsbModulator → Channelizer → SsbDemod →
//! RealF32Resamp → <fldigi block>`, with `AutoTune::estimate_center_hz`
//! finding the signal so the channelizer shift is *discovered*, not
//! hardcoded. Prints decodes over a small shift sweep so we can see
//! (a) the chain decodes a known-good mode, (b) whether navtex — which
//! never synced audio-direct — comes alive once it's a real RF signal
//! with proper tuning. Always passes; it's a measurement.

#![cfg(feature = "fldigi")]
#![allow(clippy::cast_precision_loss)]

mod common;

use common::{
    init_at, load_audio, pump_iq_to_iq, pump_iq_to_real, pump_real_to_iq, pump_real_to_real,
    sample_path,
};
use ferrite_blocks::block::{Block, BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{
    estimate_center_hz, Channelizer, ChannelizerParams, NavtexDemod, NavtexDemodParams, RttyDemod,
    RttyDemodParams, SsbDemod, SsbDemodParams, SsbModulator, SsbModulatorParams,
};
use num_complex::Complex;
use std::sync::{Mutex, OnceLock};

const A_RATE: f64 = 8_000.0;
const IQ_RATE: f64 = 48_000.0;
const OFFSET: f32 = 12_000.0;

fn guard() -> std::sync::MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn capture<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::Arc;
    use tracing_subscriber::fmt::MakeWriter;
    #[derive(Clone)]
    struct W(Arc<Mutex<Vec<u8>>>);
    impl Write for W {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for W {
        type Writer = W;
        fn make_writer(&'a self) -> W {
            self.clone()
        }
    }
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(W(Arc::clone(&buf)))
        .with_target(true)
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(sub, f);
    let b = buf.lock().unwrap();
    String::from_utf8_lossy(&b)
        .lines()
        .filter(|l| l.contains("decoder::fldigi"))
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

/// Build the SSB IQ once, report AutoTune's estimate of where it sits.
fn modulate(audio: &[f32]) -> (Vec<Complex<f32>>, f32) {
    let mut m = SsbModulator::new(SsbModulatorParams {
        input_rate_hz: A_RATE as f32,
        output_rate_hz: IQ_RATE as f32,
        offset_hz: OFFSET,
        sideband: ferrite_blocks::ssb_modulator::Sideband::Usb,
    })
    .unwrap();
    let iq = pump_real_to_iq(&mut m, audio);
    // AutoTune discovers the signal (gate generously around the offset).
    let win = &iq[iq.len() / 3..iq.len() / 3 + 16_384.min(iq.len() / 3)];
    let est = estimate_center_hz(win, IQ_RATE as f32, OFFSET - 4_000.0, OFFSET + 4_000.0)
        .unwrap_or(OFFSET);
    (iq, est)
}

/// Run the chain at a given channelizer shift, drive `block`, return
/// reassembled decode.
fn run(iq: &[Complex<f32>], shift: f64, block: &mut dyn Block) -> String {
    let mut ch = Channelizer::new(ChannelizerParams::new(IQ_RATE, shift, A_RATE)).unwrap();
    init_at(&mut ch, IQ_RATE);
    let chan = pump_iq_to_iq(&mut ch, iq);

    let mut demod = SsbDemod::new(SsbDemodParams {
        sample_rate_hz: A_RATE as f32,
        sideband: ferrite_blocks::Sideband::Usb,
        audio_gain: 6.0,
    })
    .unwrap();
    init_at(&mut demod, A_RATE);
    let aud = pump_iq_to_real(&mut demod, &chan);

    let mut rs = ferrite_blocks::RealF32Resamp::new(ferrite_blocks::RealF32ResampParams {
        output_rate_hz: A_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut rs, A_RATE);
    let aud = pump_real_to_real(&mut rs, &aud);

    capture(|| {
        let mut i = 0;
        while i < aud.len() {
            let t = 2_048.min(aud.len() - i);
            let mut ins = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&aud[i..i + t]),
            }];
            let mut outs: [OutputPort; 0] = [];
            block
                .process(&mut BlockIo {
                    inputs: &mut ins,
                    outputs: &mut outs,
                })
                .unwrap();
            i += t;
        }
    })
}

fn sweep(label: &str, fixture: &str, mk: &dyn Fn() -> Box<dyn Block>) {
    if !sample_path(fixture).exists() {
        println!("{label}: fixture missing, skip");
        return;
    }
    let (audio, _r) = load_audio(fixture);
    let (iq, est) = modulate(&audio);
    println!("\n=== {label}: AutoTune estimate {est:.0} Hz (offset {OFFSET}) ===");
    // Sweep channelizer shift around the SSB carrier (= offset) and the
    // energy estimate; carrier-vs-centroid gap ≈ half the audio BW.
    for shift in [
        f64::from(OFFSET),
        f64::from(OFFSET) + 500.0,
        f64::from(OFFSET) + 1_000.0,
        f64::from(est),
        f64::from(est) - 500.0,
        f64::from(OFFSET) - 500.0,
    ] {
        let mut b = mk();
        let txt = run(&iq, shift, b.as_mut());
        let printable: String = txt.chars().filter(|c| !c.is_control()).collect();
        let hit = if printable.to_uppercase().contains("QUICK BROWN FOX") {
            " <<< PANGRAM"
        } else {
            ""
        };
        println!(
            "  shift {shift:>7.0}: {:?}{hit}",
            printable.chars().take(60).collect::<String>()
        );
    }
}

#[test]
fn probe_fullchain_rtty_and_navtex() {
    let _g = guard();
    sweep(
        "rtty45",
        "sigidwiki/8000_mono/RTTY_170Hz_45.45bd.wav",
        &|| Box::new(RttyDemod::new(RttyDemodParams::default()).unwrap()),
    );
    sweep("navtex", "sigidwiki/8000_mono/NAVTEX_SITOR-B.wav", &|| {
        Box::new(NavtexDemod::new(NavtexDemodParams::default()).unwrap())
    });
}
