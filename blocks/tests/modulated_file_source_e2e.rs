//! `ModulatedFileSource` as a drop-in source on real curated samples.
//!
//! Proves the File-tab feature end to end: a *single* source block
//! replays a known sample as a real RF signal through the production
//! RX chain.
//!
//! * **audio path** — `Olivia_8-500.wav` (a curated sigidwiki
//!   fixture) → `ModulatedFileSource{kind:audio, ssb}` →
//!   `Channelizer → SsbDemod → RealF32Resamp → OliviaDemod` → decodes
//!   the pangram.
//! * **iq path** — the APRS IQ capture → `ModulatedFileSource{kind:iq,
//!   offset}` → finite IQ with energy at the requested rate.

#![cfg(feature = "fldigi")]
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use common::{init_at, pump_iq_to_iq, pump_iq_to_real, pump_real_to_real, sample_path};
use ferrite_blocks::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    Channelizer, ChannelizerParams, FileIqSource, FileIqSourceParams, ModulatedFileSource,
    ModulatedFileSourceParams, OliviaDemod, OliviaDemodParams, RealF32Resamp, RealF32ResampParams,
    SsbDemod, SsbDemodParams,
};
use num_complex::Complex;

const A_RATE: f64 = 8_000.0;
const IQ_RATE: f64 = 48_000.0;
const OFFSET: f64 = 12_000.0;

/// Drain a source block (no inputs) to EOF.
fn drain_source(src: &mut dyn Block) -> Vec<Complex<f32>> {
    let mut all = Vec::new();
    let mut buf = vec![Complex::new(0.0_f32, 0.0); 1 << 15];
    loop {
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut buf),
        }];
        let w = src
            .process(&mut BlockIo {
                inputs: &mut [],
                outputs: &mut outs,
            })
            .unwrap();
        if w.produced[0] == 0 {
            break;
        }
        all.extend_from_slice(&buf[..w.produced[0]]);
        if all.len() > 6_000_000 {
            break; // safety — non-looping fixtures end well before this
        }
    }
    all
}

fn with_fldigi_capture<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
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
    let bytes = buf.lock().unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| l.contains("decoder::fldigi"))
        .filter_map(|l| {
            l.split("decoder::fldigi: ")
                .nth(1)
                .map(|b| b.rsplit_once(" mode=").map_or(b, |(t, _)| t).to_string())
        })
        .collect()
}

#[test]
fn audio_sample_modulated_through_full_chain_decodes() {
    let path = sample_path("sigidwiki/8000_mono/Olivia_8-500.wav");
    assert!(path.exists(), "fixture missing — run to_fldigi_8k.sh");

    // One source block replaces FileAudioSource+SsbModulator.
    let mut src = ModulatedFileSource::new(ModulatedFileSourceParams {
        path,
        kind: "audio".into(),
        modulation: "ssb".into(),
        sideband: "usb".into(),
        offset_hz: OFFSET as f32,
        deviation_hz: 5_000.0,
        mod_depth: 0.7,
        output_rate_hz: IQ_RATE as f32,
        rate_hz_hint: 0.0,
        center_freq_hz: 0.0,
        loop_playback: false,
    })
    .unwrap();
    let iq = drain_source(&mut src);
    assert!(iq.len() > 100_000, "too little IQ ({}) ", iq.len());

    let mut ch = Channelizer::new(ChannelizerParams::new(IQ_RATE, OFFSET, A_RATE)).unwrap();
    init_at(&mut ch, IQ_RATE);
    let chan = pump_iq_to_iq(&mut ch, &iq);
    let mut sd = SsbDemod::new(SsbDemodParams {
        sample_rate_hz: A_RATE as f32,
        sideband: ferrite_blocks::Sideband::Usb,
        audio_gain: 6.0,
    })
    .unwrap();
    init_at(&mut sd, A_RATE);
    let da = pump_iq_to_real(&mut sd, &chan);
    let mut rs = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: A_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut rs, A_RATE);
    let rx = pump_real_to_real(&mut rs, &da);

    let mut dec = OliviaDemod::new(OliviaDemodParams::default()).unwrap();
    let text = with_fldigi_capture(|| {
        let mut i = 0;
        while i < rx.len() {
            let t = 2_048.min(rx.len() - i);
            let mut ins = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&rx[i..i + t]),
            }];
            let mut outs: [OutputPort; 0] = [];
            dec.process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
            i += t;
        }
    });
    assert!(
        text.to_uppercase().contains("QUICK BROWN FOX"),
        "ModulatedFileSource(audio/ssb) → chain → Olivia didn't decode (got {text:?})"
    );
}

#[test]
fn iq_sample_upmixed_produces_signal() {
    let path = sample_path("vhf/aprs_145.070mhz_iq-s16.wav");
    assert!(path.exists(), "APRS IQ fixture missing");
    let mut src = ModulatedFileSource::new(ModulatedFileSourceParams {
        path,
        kind: "iq".into(),
        modulation: "fm".into(), // ignored for iq
        sideband: "usb".into(),
        offset_hz: 8_000.0, // shift off DC → channelizer must tune
        deviation_hz: 5_000.0,
        mod_depth: 0.7,
        output_rate_hz: 39_062.0, // = the capture rate (IqUpmix is 1:1)
        rate_hz_hint: 0.0,
        center_freq_hz: 145_070_000.0,
        loop_playback: false,
    })
    .unwrap();
    let iq = drain_source(&mut src);
    assert!(iq.len() > 100_000, "too little IQ ({})", iq.len());
    assert!(iq.iter().all(|c| c.re.is_finite() && c.im.is_finite()));

    // IqUpmix is a pure NCO rotation: |out| == |in| sample-for-sample, so
    // total energy must match a bare FileIqSource read of the same file.
    // (An absolute floor is wrong here — APRS is bursty/low-level, mostly
    // band noise between packets.)
    let mut ref_src = FileIqSource::new(&FileIqSourceParams {
        path: sample_path("vhf/aprs_145.070mhz_iq-s16.wav"),
        rate_hz_hint: 0.0,
        center_freq_hz: 145_070_000.0,
        loop_playback: false,
    })
    .unwrap();
    let raw = drain_source(&mut ref_src);
    let mean = |v: &[Complex<f32>]| {
        v.iter().map(num_complex::Complex::norm_sqr).sum::<f32>() / v.len().max(1) as f32
    };
    let (p, p_ref) = (mean(&iq), mean(&raw));
    assert!(p_ref > 0.0, "reference capture is silent — bad fixture");
    let ratio = p / p_ref;
    assert!(
        (0.5..=1.5).contains(&ratio),
        "ModulatedFileSource(iq) energy {p} != FileIqSource {p_ref} (ratio {ratio})"
    );
}
