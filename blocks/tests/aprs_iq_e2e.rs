//! End-to-end APRS decode from the *real off-air IQ capture*, through
//! the full RF→audio chain.
//!
//! `packet_e2e` feeds the sigidwiki AFSK1200 *audio* clip straight into
//! `PacketDemod`. This test instead starts from
//! `samples/vhf/aprs_145.070mhz_iq-s16.wav` (39 062 Hz stereo s16, L=I
//! R=Q — the original SDR# capture) and runs the exact listen chain:
//! `FileIqSource → Channelizer → FmDemod → RealF32Resamp → PacketDemod`.
//! The gate is the same — ≥1 frame into `decoder::packet` — but now it
//! exercises the tuner, FM discriminator, and resampler too, not just
//! the AFSK modem. This is the real-RF complement to `packet_e2e`.

#![cfg(feature = "multimon")]
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use common::{init_at, pump_iq_to_real, pump_real_to_real, sample_path};
use ferrite_blocks::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    Channelizer, ChannelizerParams, FileIqSource, FileIqSourceParams, FmDemod, FmDemodParams,
    PacketDemod, PacketDemodParams, RealF32Resamp, RealF32ResampParams,
};
use num_complex::Complex;

const IN_RATE: f64 = 39_062.0;
/// 39062 / round(39062/19531) = ÷2 = 19531 Hz NBFM channel.
const CH_RATE: f64 = 39_062.0 / 2.0;
/// multimon's AFSK1200 decoder (as used by `packet_e2e`) runs at
/// 22 050 Hz; resample the recovered audio to match.
const DEMOD_RATE: f64 = 22_050.0;
/// Bell 202 over 2 m FM keys roughly ±3 kHz.
const APRS_DEV: f32 = 3_000.0;

fn load_iq() -> Vec<Complex<f32>> {
    let mut src = FileIqSource::new(&FileIqSourceParams {
        path: sample_path("vhf/aprs_145.070mhz_iq-s16.wav"),
        rate_hz_hint: 0.0,
        center_freq_hz: 145_070_000.0,
        loop_playback: false,
    })
    .expect("open APRS IQ capture");
    assert!((src.rate_hz() - IN_RATE).abs() < 1.0, "expected 39062 Hz");
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
        all.extend_from_slice(&buf[..w.produced[0]]);
        if src.is_eof() {
            break;
        }
    }
    all
}

fn with_packet_capture<F: FnOnce()>(f: F) -> Vec<String> {
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
        .filter(|l| l.contains("decoder::packet"))
        .map(str::to_string)
        .collect()
}

#[test]
fn aprs_decodes_from_real_iq_capture() {
    let iq = load_iq();
    assert!(iq.len() > 100_000, "APRS capture unexpectedly short");

    // Tuner: capture is centred on the signal → no shift.
    let mut ch = Channelizer::new(ChannelizerParams::new(IN_RATE, 0.0, CH_RATE)).unwrap();
    init_at(&mut ch, IN_RATE);
    let chan = common::pump_iq_to_iq(&mut ch, &iq);

    let mut fm = FmDemod::new(FmDemodParams {
        sample_rate_hz: CH_RATE as f32,
        max_deviation_hz: APRS_DEV,
    })
    .unwrap();
    init_at(&mut fm, CH_RATE);
    let if_audio = pump_iq_to_real(&mut fm, &chan);

    let mut rs = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: DEMOD_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut rs, CH_RATE);
    let audio = pump_real_to_real(&mut rs, &if_audio);
    assert!(
        audio.iter().all(|x| x.is_finite()),
        "non-finite audio into PacketDemod"
    );

    // General AX.25 monitor mode (aprs_mode off): the 145.07 capture
    // is connected-mode / BBS packet, not APRS UI/0xF0 — multimon's
    // APRS display suppresses non-UI frames entirely. This test is the
    // RF-chain smoke test; the APRS-events path is covered by
    // packet_e2e's contract test (which keeps the default aprs_mode).
    let mut pkt = PacketDemod::new(PacketDemodParams {
        aprs_mode: false,
        ..PacketDemodParams::default()
    })
    .expect("packet demod");
    let lines = with_packet_capture(|| {
        let mut idx = 0;
        while idx < audio.len() {
            let take = 4_096.min(audio.len() - idx);
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&audio[idx..idx + take]),
            }];
            let mut outputs: [OutputPort; 0] = [];
            pkt.process(&mut BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            })
            .unwrap();
            idx += take;
        }
    });

    println!("decoder::packet lines: {}", lines.len());
    for l in lines.iter().take(3) {
        println!("  {l}");
    }
    assert!(
        !lines.is_empty(),
        "expected ≥1 decoder::packet frame from the real APRS IQ chain, got 0 \
         ({} audio samples @ {DEMOD_RATE} Hz)",
        audio.len()
    );
}
