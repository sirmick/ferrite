//! Probe: do the sigidwiki fixtures carry an RSID burst, and does
//! FldigiAuto auto-switch on it?
//!
//! Each fixture runs the full RX flowgraph into a `FldigiAuto` STARTED
//! IN THE WRONG MODE (rtty45). If the recording begins with an RSID
//! (common ham practice for Olivia/Contestia/MT63), `decoder::rsid`
//! should fire and the post-switch decode should produce the pangram.
//! Always passes — it's a measurement to pick the e2e fixture.

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
use std::sync::{Mutex, OnceLock};

const A_RATE: f64 = 8_000.0;
const IQ_RATE: f64 = 48_000.0;
const OFFSET: f64 = 12_000.0;

fn guard() -> std::sync::MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn capture<F: FnOnce()>(f: F) -> (Vec<String>, String) {
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
    let s = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    let rsid: Vec<String> = s
        .lines()
        .filter(|l| l.contains("decoder::rsid"))
        .map(str::to_string)
        .collect();
    let text: String = s
        .lines()
        .filter(|l| l.contains("decoder::fldigi"))
        .filter_map(|l| {
            l.split("decoder::fldigi: ")
                .nth(1)
                .map(|b| b.rsplit_once(" mode=").map_or(b, |(t, _)| t).to_string())
        })
        .collect();
    (rsid, text)
}

fn probe(base: &str, shift: f64) {
    if !sample_path(&format!("sigidwiki/8000_mono/{base}.wav")).exists() {
        println!("{base}: missing");
        return;
    }
    let _g = guard();
    let (audio, _r) = load_audio(&format!("sigidwiki/8000_mono/{base}.wav"));
    let mut m = SsbModulator::new(SsbModulatorParams {
        input_rate_hz: A_RATE as f32,
        output_rate_hz: IQ_RATE as f32,
        offset_hz: OFFSET as f32,
        sideband: ferrite_blocks::ssb_modulator::Sideband::Usb,
    })
    .unwrap();
    let iq = pump_real_to_iq(&mut m, &audio);
    let mut ch = Channelizer::new(ChannelizerParams::new(IQ_RATE, shift, A_RATE)).unwrap();
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

    // Start in the WRONG mode so any RSID switch is observable.
    let mut b = FldigiAuto::new(FldigiAutoParams {
        start_mode: "rtty45".to_string(),
        afc: true,
        rx_freq_hz: 0.0,
    })
    .unwrap();
    let (rsid, text) = capture(|| {
        let mut i = 0;
        while i < rx.len() {
            let t = 2_048.min(rx.len() - i);
            let mut ins = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&rx[i..i + t]),
            }];
            let mut outs: [OutputPort; 0] = [];
            b.process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
            i += t;
        }
    });
    let pang = text.to_uppercase().contains("QUICK BROWN FOX");
    println!(
        "{base:>20}: rsid={:?} pangram={pang}",
        rsid.iter()
            .filter_map(|l| l.split("decoder::rsid:").nth(1).map(str::trim))
            .collect::<Vec<_>>()
    );
}

#[test]
fn probe_fixtures_for_rsid() {
    // (fixture, channelizer shift that decodes that mode post-switch)
    for (base, shift) in [
        ("RTTY_170Hz_45.45bd", OFFSET - 500.0),
        ("BPSK31", OFFSET),
        ("Olivia_8-500", OFFSET),
        ("Contestia_8-500", OFFSET),
        ("THROB4", OFFSET),
        ("MT63-1000L", OFFSET + 500.0),
        ("DominoEX_16Bd", OFFSET - 250.0),
        ("NAVTEX_SITOR-B", OFFSET),
    ] {
        probe(base, shift);
    }
}
