//! e2e: FldigiAuto RSID-driven mode switch through the full RX chain.
//!
//! `FileAudioSource(Olivia) → SsbModulator → Channelizer → SsbDemod →
//! RealF32Resamp → FldigiAuto`, with FldigiAuto **started in the wrong
//! mode** (`rtty45`). An RSID detection for `olivia-8-500` is injected
//! (the shim test seam — see below); FldigiAuto must auto-switch and
//! then decode the Olivia pangram.
//!
//! Scope/honesty: this validates *our* integration end to end —
//! detection queue → `take_rsid` → `FldigiCore::switch_mode` →
//! `FldigiAuto` re-arm → decode — exactly the path a real cRsId hit
//! drives. It does NOT re-test fldigi's Reed-Solomon *detector*
//! itself: that's upstream-tested vendored DSP, and exercising it
//! would require fldigi's RS *encoder* (private in the vendored
//! header). The injection puts the identical string a real hit
//! queues, so everything downstream of cRsId is covered.

#![cfg(feature = "fldigi")]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)]

mod common;

use common::{
    init_at, load_audio, pump_iq_to_iq, pump_iq_to_real, pump_real_to_iq, pump_real_to_real,
    sample_path,
};
use ferrite_blocks::block::{Block, BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{
    Channelizer, ChannelizerParams, FldigiAuto, FldigiAutoParams, RealF32Resamp,
    RealF32ResampParams, SsbDemod, SsbDemodParams, SsbModulator, SsbModulatorParams,
};

const A_RATE: f64 = 8_000.0;
const IQ_RATE: f64 = 48_000.0;
const OFFSET: f64 = 12_000.0;

fn capture<F: FnOnce()>(f: F) -> (bool, String) {
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
    let s = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    // RSID switch line: `decoder::rsid: RSID from=rtty45 to=olivia-8-500`
    let switched = s
        .lines()
        .any(|l| l.contains("decoder::rsid") && l.contains("to=olivia-8-500"));
    let text: String = s
        .lines()
        .filter(|l| l.contains("decoder::fldigi"))
        .filter_map(|l| {
            l.split("decoder::fldigi: ")
                .nth(1)
                .map(|b| b.rsplit_once(" mode=").map_or(b, |(t, _)| t).to_string())
        })
        .collect();
    (switched, text)
}

#[test]
fn fldigi_auto_rsid_switches_and_decodes() {
    let rel = "sigidwiki/8000_mono/Olivia_8-500.wav";
    assert!(
        sample_path(rel).exists(),
        "{rel} missing — run samples/sigidwiki/to_fldigi_8k.sh"
    );
    let (audio, rate) = load_audio(rel);
    assert!((rate - A_RATE).abs() < 1.0);

    // Full RX chain (Olivia decodes at shift = OFFSET — bias 0).
    let mut modu = SsbModulator::new(SsbModulatorParams {
        input_rate_hz: A_RATE as f32,
        output_rate_hz: IQ_RATE as f32,
        offset_hz: OFFSET as f32,
        sideband: ferrite_blocks::ssb_modulator::Sideband::Usb,
    })
    .unwrap();
    let iq = pump_real_to_iq(&mut modu, &audio);
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

    // ~0.5 s of leading silence so the (wrong-mode) first chunk + the
    // switch happen before real Olivia audio — the whole signal is
    // then decoded in olivia mode.
    let mut feed = vec![0.0_f32; 4_096];
    feed.extend_from_slice(&rx);

    let mut auto = FldigiAuto::new(FldigiAutoParams {
        start_mode: "rtty45".to_string(),
        afc: true,
        rx_freq_hz: 0.0,
    })
    .unwrap();
    // Simulate the RSID hit a transmitting station would send up front.
    auto.inject_rsid("olivia-8-500");

    let (switched, text) = capture(|| {
        let mut i = 0;
        while i < feed.len() {
            let t = 2_048.min(feed.len() - i);
            let mut ins = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&feed[i..i + t]),
            }];
            let mut outs: [OutputPort; 0] = [];
            auto.process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
            i += t;
        }
    });

    assert!(
        switched,
        "FldigiAuto did not log the rtty45→olivia-8-500 RSID switch"
    );
    assert!(
        text.to_uppercase().contains("QUICK BROWN FOX"),
        "auto-switched Olivia did not decode the pangram (got {text:?})"
    );
}
