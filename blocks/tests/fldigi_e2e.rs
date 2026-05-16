//! Synthetic RTTY decode through the **full RX flowgraph**.
//!
//! Synthesises a deterministic Baudot/ITA2 RTTY audio signal (fldigi
//! RX defaults — `blocks/native/fldigi/tests/rtty_decode.rs` for the
//! convention), then runs it through the production path rather than
//! straight into the modem:
//!
//! ```text
//! synth audio → SsbModulator → Channelizer → SsbDemod →
//! RealF32Resamp → RttyDemod
//! ```
//!
//! so the tuner / resampler / demod are exercised, and `RttyDemod`
//! takes **no `rx_freq_hz`** — like `fldigi_modes_e2e`, tuning is the
//! channelizer shift. The synthetic is generated at fldigi's RTTY
//! sweet spot (`CENTER` 1500 Hz), so the round-trip is transparent at
//! `shift = OFFSET` (no bias — unlike the off-centre sigidwiki
//! recording). Asserts the decode reaches `decoder::fldigi` (the UI
//! transport). Decode-core coverage is the crate-level
//! `rtty_decode.rs`; this is the block + chain + tracing plumbing.

#![cfg(feature = "fldigi")]
// Synthetic RTTY generator math (sample counts from baud/shift) — same
// rounding casts the other *_e2e signal generators allow.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

mod common;

use std::f32::consts::PI;

use common::{init_at, pump_iq_to_iq, pump_iq_to_real, pump_real_to_iq, pump_real_to_real};
use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{
    Block, Channelizer, ChannelizerParams, RealF32Resamp, RealF32ResampParams, RttyDemod,
    RttyDemodParams, SsbDemod, SsbDemodParams, SsbModulator, SsbModulatorParams,
};

const A_RATE: f64 = 8_000.0;
const IQ_RATE: f64 = 48_000.0;
const OFFSET: f64 = 12_000.0;

const FS: f32 = 8_000.0;
const CENTER: f32 = 1_500.0;
const SHIFT: f32 = 170.0;
const BAUD: f32 = 45.0;

// fldigi vendor/rtty.cxx letters[32]: index = 5-bit Baudot code.
const LETTERS: [u8; 32] = [
    0, b'E', b'\n', b'A', b' ', b'S', b'I', b'U', b'\r', b'D', b'R', b'J', b'N', b'F', b'C', b'K',
    b'T', b'Z', b'L', b'W', b'H', b'Y', b'P', b'Q', b'O', b'B', b'G', 0, b'M', b'X', b'V', 0,
];
const LTRS: u8 = 0b11111;
const BAUDOT_SPACE: u8 = 0b00100;

fn code_for(c: u8) -> u8 {
    (0u8..32)
        .find(|&i| LETTERS[i as usize] == c)
        .unwrap_or(LTRS)
}

fn push_char(out: &mut Vec<f32>, phase: &mut f32, code: u8) {
    let emit = |bit: bool, bits: f32, out: &mut Vec<f32>, phase: &mut f32| {
        let f = if bit {
            CENTER + SHIFT / 2.0
        } else {
            CENTER - SHIFT / 2.0
        };
        let n = (FS / BAUD * bits).round() as usize;
        for _ in 0..n {
            *phase += 2.0 * PI * f / FS;
            if *phase > 2.0 * PI {
                *phase -= 2.0 * PI;
            }
            out.push(phase.sin() * 0.5);
        }
    };
    emit(false, 1.0, out, phase); // start (space)
    for b in 0..5 {
        emit((code >> b) & 1 == 1, 1.0, out, phase); // 5 data, LSB-first
    }
    emit(true, 1.5, out, phase); // 1.5 stop (mark)
}

fn modulate(text: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut phase = 0.0f32;
    for _ in 0..(FS * 0.5) as usize {
        phase += 2.0 * PI * (CENTER + SHIFT / 2.0) / FS;
        out.push(phase.sin() * 0.5);
    }
    for _ in 0..8 {
        push_char(&mut out, &mut phase, LTRS);
    }
    for &c in text.as_bytes() {
        push_char(
            &mut out,
            &mut phase,
            if c == b' ' { BAUDOT_SPACE } else { code_for(c) },
        );
    }
    for _ in 0..4 {
        push_char(&mut out, &mut phase, LTRS);
    }
    out
}

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

#[test]
fn rtty_through_full_flowgraph_reaches_decoder_tracing() {
    let audio = modulate("CQ CQ DE FERRITE FERRITE K");

    // synth audio → SSB IQ at OFFSET → channelize back → SSB demod →
    // resample to 8 kHz. Synthetic is at CENTER=1500 (fldigi's RTTY
    // sweet spot), so shift = OFFSET is the transparent round-trip.
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

    let mut sdem = SsbDemod::new(SsbDemodParams {
        sample_rate_hz: A_RATE as f32,
        sideband: ferrite_blocks::Sideband::Usb,
        audio_gain: 6.0,
    })
    .unwrap();
    init_at(&mut sdem, A_RATE);
    let demod_audio = pump_iq_to_real(&mut sdem, &chan);

    let mut rs = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: A_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut rs, A_RATE);
    let rx_audio = pump_real_to_real(&mut rs, &demod_audio);

    let mut block =
        RttyDemod::new(RttyDemodParams::default()).expect("RttyDemod rtty45 constructs");

    let lines = with_fldigi_capture(|| {
        let mut idx = 0;
        while idx < rx_audio.len() {
            let take = 2_048.min(rx_audio.len() - idx);
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&rx_audio[idx..idx + take]),
            }];
            let mut outputs: [OutputPort; 0] = [];
            let mut io = BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            };
            block.process(&mut io).unwrap();
            idx += take;
        }
    });

    // The block emits decoded text incrementally (whatever a tick
    // drained → one tracing line each), so reassemble the stream: take
    // the slice between "decoder::fldigi: " and " mode=" on each line
    // and concatenate.
    let decoded: String = lines
        .iter()
        .filter_map(|l| {
            let body = l.split("decoder::fldigi: ").nth(1)?;
            Some(
                body.rsplit_once(" mode=")
                    .map_or(body, |(t, _)| t)
                    .to_string(),
            )
        })
        .collect::<String>()
        .to_uppercase();

    assert!(
        decoded.contains("FERRITE"),
        "expected reassembled decoder::fldigi stream to contain FERRITE, got {decoded:?} (lines={lines:?})"
    );
}
