//! DTMF decoder — real audio → JSON digit events.
//!
//! Eight Goertzel filters detect the standard ITU-T Q.23 row/column
//! tone pair; a small state machine holds the result until a digit has
//! been seen for `hold_ms` without interruption, then emits one event
//! per digit on release. Output is newline-delimited JSON, one event
//! per line, on the block's `events` port. Downstream `EventsSink`
//! routes those bytes to a test queue (native) or the main thread
//! (browser).
//!
//! ### Goertzel
//!
//! For each of the 8 target frequencies we maintain
//!
//! ```text
//! s0 = x + k·s1 − s2
//! s2 = s1
//! s1 = s0
//! ```
//!
//! with `k = 2·cos(2π·f/fs)`. Over `block_size` samples this sums the
//! energy at frequency `f`; `power = s1² + s2² − k·s1·s2`. We reset the
//! state every `block_size` samples and compare the eight powers to
//! pick the row and column tone.
//!
//! ### State machine
//!
//! ```text
//! Idle ──(tone)──▶ Detecting(d)
//!                      │
//!    held ≥ hold_ms ───┼───▶ Confirmed(d, t_start)
//!                                     │
//!                        (silence ≥ off_ms | new_d)
//!                                     ▼
//!                               emit{d, duration_ms, t_ms}
//! ```

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};

const ROW_FREQS: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
const COL_FREQS: [f32; 4] = [1_209.0, 1_336.0, 1_477.0, 1_633.0];
const KEYPAD: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DtmfDecoderParams {
    pub sample_rate_hz: f32,
    /// Goertzel block length in samples. Sets detection granularity and
    /// frequency resolution (`fs/N`). At fs=8 kHz, N=200 gives 40 Hz
    /// bins and ~25 ms of latency — finer than the 89 Hz closest DTMF
    /// spacing.
    pub block_size: u32,
    /// Minimum tone duration for a detection to count as a digit press.
    pub hold_ms: u32,
    /// Minimum silence between re-emits of the same digit.
    pub off_ms: u32,
    /// Strongest-to-rest ratio required for a row/col tone to "win".
    /// Power of the strongest tone must exceed `dominance × sum(other 3)`.
    pub dominance: f32,
}

impl Default for DtmfDecoderParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 8_000.0,
            block_size: 200,
            hold_ms: 40,
            off_ms: 40,
            dominance: 4.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// A digit candidate is being held; `started_at_sample` is the
    /// global sample index when the hold began.
    Holding {
        digit: char,
        started_at_sample: u64,
    },
}

pub struct DtmfDecoder {
    params: DtmfDecoderParams,
    /// Goertzel coefficients `2·cos(2π·f/fs)` per tone.
    coeff: [f32; 8],
    /// Goertzel state registers per tone.
    s1: [f32; 8],
    s2: [f32; 8],
    /// How many samples we've fed the current Goertzel block.
    block_samples_in: u32,
    /// Sample counter across the entire run — for `t_ms` timestamps.
    total_samples: u64,
    state: State,
    /// Samples since the most recent confirmed digit was released; used
    /// to gate same-digit re-detection.
    samples_since_release: u64,
    /// Most recently released digit; `off_ms` silence is required
    /// before we'll emit the same digit again.
    last_released: Option<char>,
    /// Buffered event bytes (NDJSON) waiting to be drained to the
    /// output port on the next `process` call.
    pending: Vec<u8>,
}

impl DtmfDecoder {
    pub fn new(params: DtmfDecoderParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!("dtmf_decoder sample_rate_hz must be > 0");
        }
        if params.block_size < 32 {
            bail!("dtmf_decoder block_size must be >= 32");
        }
        let mut coeff = [0.0_f32; 8];
        for (i, f) in ROW_FREQS.iter().chain(COL_FREQS.iter()).enumerate() {
            coeff[i] = 2.0 * (core::f32::consts::TAU * f / params.sample_rate_hz).cos();
        }
        Ok(Self {
            params,
            coeff,
            s1: [0.0; 8],
            s2: [0.0; 8],
            block_samples_in: 0,
            total_samples: 0,
            state: State::Idle,
            samples_since_release: u64::MAX / 2,
            last_released: None,
            pending: Vec::new(),
        })
    }

    /// Advance the 8 Goertzel filters by one sample.
    fn feed_sample(&mut self, x: f32) {
        for i in 0..8 {
            let s0 = x + self.coeff[i] * self.s1[i] - self.s2[i];
            self.s2[i] = self.s1[i];
            self.s1[i] = s0;
        }
    }

    /// Compute per-tone power, reset state, return the digit (if any)
    /// that this block represents.
    fn flush_block(&mut self) -> Option<char> {
        let mut powers = [0.0_f32; 8];
        for (i, p) in powers.iter_mut().enumerate() {
            *p = self.s1[i] * self.s1[i] + self.s2[i] * self.s2[i]
                - self.coeff[i] * self.s1[i] * self.s2[i];
            self.s1[i] = 0.0;
            self.s2[i] = 0.0;
        }
        self.block_samples_in = 0;

        let (row_idx, row_power) = argmax(&powers[0..4]);
        let (col_idx, col_power) = argmax(&powers[4..8]);
        let row_rest: f32 = powers[0..4].iter().sum::<f32>() - row_power;
        let col_rest: f32 = powers[4..8].iter().sum::<f32>() - col_power;

        // Dominance: strongest tone must exceed `dominance × sum of the
        // other three tones in its group`. A low absolute threshold is
        // baked in so pure silence doesn't decode as a digit.
        let min_power = 1e-6_f32;
        let row_ok = row_power > min_power && row_power > self.params.dominance * row_rest;
        let col_ok = col_power > min_power && col_power > self.params.dominance * col_rest;
        if !(row_ok && col_ok) {
            return None;
        }
        Some(KEYPAD[row_idx][col_idx])
    }

    fn sample_index_to_ms(&self, sample_idx: u64) -> u64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = ((sample_idx as f64) * 1000.0 / f64::from(self.params.sample_rate_hz)) as u64;
        ms
    }

    /// Handle one block's detection result. May append to `self.pending`.
    fn handle_block(&mut self, detected: Option<char>) {
        let block = u64::from(self.params.block_size);
        match (self.state, detected) {
            (State::Idle, None) => {
                self.samples_since_release = self.samples_since_release.saturating_add(block);
            }
            (State::Idle, Some(d)) => {
                // Gate same-digit re-detection on an off gap.
                let off_samples = self.ms_to_samples(self.params.off_ms);
                if self.last_released == Some(d) && self.samples_since_release < off_samples {
                    // Still in gap — ignore.
                } else {
                    self.state = State::Holding {
                        digit: d,
                        // The digit "started" at the beginning of the
                        // block that just closed.
                        started_at_sample: self.total_samples - block,
                    };
                }
            }
            (
                State::Holding {
                    digit,
                    started_at_sample,
                },
                Some(d),
            ) if d == digit => {
                // Still the same digit — keep holding.
                let _ = (digit, started_at_sample);
            }
            (
                State::Holding {
                    digit,
                    started_at_sample,
                },
                other,
            ) => {
                // Digit changed or dropped out. Release if it was held
                // long enough.
                let held_samples = self.total_samples - block - started_at_sample;
                let hold_samples = self.ms_to_samples(self.params.hold_ms);
                if held_samples >= hold_samples {
                    let duration_ms = self.sample_index_to_ms(held_samples);
                    let t_ms = self.sample_index_to_ms(started_at_sample);
                    self.emit_event(digit, duration_ms, t_ms);
                    self.last_released = Some(digit);
                    self.samples_since_release = block;
                } else {
                    self.samples_since_release = self.samples_since_release.saturating_add(block);
                }
                self.state = match other {
                    Some(d) => State::Holding {
                        digit: d,
                        started_at_sample: self.total_samples - block,
                    },
                    None => State::Idle,
                };
            }
        }
    }

    fn ms_to_samples(&self, ms: u32) -> u64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let s = ((f64::from(ms) * f64::from(self.params.sample_rate_hz)) / 1000.0).round() as u64;
        s
    }

    fn emit_event(&mut self, digit: char, duration_ms: u64, t_ms: u64) {
        // Compact JSON by hand — the payload is small and fully
        // controlled here, so pulling in serde_json for one line is
        // overkill. Newline-terminated: the sink splits on '\n'.
        self.pending
            .extend_from_slice(b"{\"kind\":\"dtmf\",\"digit\":\"");
        self.pending.push(digit as u8);
        self.pending.extend_from_slice(b"\",\"duration_ms\":");
        itoa_u64_into(duration_ms, &mut self.pending);
        self.pending.extend_from_slice(b",\"t_ms\":");
        itoa_u64_into(t_ms, &mut self.pending);
        self.pending.extend_from_slice(b"}\n");
    }

    /// Test hook — drain any currently-buffered event bytes.
    #[must_use]
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn argmax(xs: &[f32]) -> (usize, f32) {
    let mut best_i = 0;
    let mut best_v = xs[0];
    for (i, &v) in xs.iter().enumerate().skip(1) {
        if v > best_v {
            best_v = v;
            best_i = i;
        }
    }
    (best_i, best_v)
}

/// Append a base-10 decimal representation of `n` to `buf` without
/// pulling in a formatter. ~7 lines, hot-path-friendly.
fn itoa_u64_into(mut n: u64, buf: &mut Vec<u8>) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let start = buf.len();
    while n > 0 {
        #[allow(clippy::cast_possible_truncation)]
        buf.push(b'0' + (n % 10) as u8);
        n /= 10;
    }
    buf[start..].reverse();
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DtmfDecoder {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DtmfDecoder",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::Events,
            }],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::EnumNumeric {
                        values: &[8_000.0, 16_000.0, 22_050.0, 48_000.0],
                        default: 8_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio rate. 8 kHz is enough — DTMF tones are 697–1633 Hz.",
                },
                ParamSpec {
                    key: "block_size",
                    label: "Goertzel block",
                    kind: ParamKind::Range {
                        min: 32.0,
                        max: 2_048.0,
                        step: 1.0,
                        default: 200.0,
                        unit: "samples",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Goertzel detection window. Larger = better SNR + more latency; 200 samples ≈ 25 ms at 8 kHz.",
                },
                ParamSpec {
                    key: "hold_ms",
                    label: "Hold time",
                    kind: ParamKind::Range {
                        min: 10.0,
                        max: 500.0,
                        step: 1.0,
                        default: 40.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Minimum continuous detection time before a digit is emitted. ITU minimum tone duration is 40 ms.",
                },
                ParamSpec {
                    key: "off_ms",
                    label: "Off gap",
                    kind: ParamKind::Range {
                        min: 10.0,
                        max: 500.0,
                        step: 1.0,
                        default: 40.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Minimum gap between digits — guards against double-emissions on a single sustained press.",
                },
                ParamSpec {
                    key: "dominance",
                    label: "Dominance ratio",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 32.0,
                        step: 0.1,
                        default: 4.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "How much louder the two DTMF tones must be than any other bin to register. Lower = more sensitive but more false positives on speech.",
                },
            ],
            ai_notes: "DTMF (touch-tone) decoder using Goertzel filters on the eight DTMF frequencies. Decodes digits from phone-call audio or any 0–9*#ABCD-keyed signal. Output: `tail decoder --category dtmf`.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        // Variable-rate: emits event bytes only on tone transitions,
        // not per-sample. Scheduler falls back on `forecast` for sizing.
        (0, 0)
    }

    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> {
        // We want to run one full Goertzel window per call so the state
        // machine sees a single, coherent power estimate instead of a
        // partial one. Below `block_size` input samples there is nothing
        // useful we can do.
        let mut f = [0; MAX_PORTS];
        f[0] = self.params.block_size as usize;
        Some(f)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(Work::new());
        };

        let n = src.len();
        for &sample in src {
            self.feed_sample(sample);
            self.block_samples_in += 1;
            self.total_samples += 1;
            if self.block_samples_in == self.params.block_size {
                let detected = self.flush_block();
                self.handle_block(detected);
            }
        }

        // Drain pending event bytes into the output, if connected. We
        // only drain what fits; leftover stays buffered for next call.
        let mut produced_events = 0;
        if let Some(dst_port) = io.outputs.iter_mut().find(|p| p.name == "out") {
            if let crate::block::OutBuf::Events(dst) = &mut dst_port.buf {
                let take = self.pending.len().min(dst.len());
                if take > 0 {
                    dst[..take].copy_from_slice(&self.pending[..take]);
                    self.pending.drain(..take);
                    produced_events = take;
                }
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = produced_events;
        Ok(w)
    }
}

impl BlockFactory for DtmfDecoder {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DtmfDecoderParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DtmfDecoder::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{DtmfDecoder, DtmfDecoderParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use crate::dtmf_audio_source::{DtmfAudioSource, DtmfAudioSourceParams};
    use core::f32::consts::TAU;

    fn gen_digit(digit: char, n: usize, fs: f32) -> Vec<f32> {
        let (row, col) = match digit {
            '1' => (697.0, 1209.0),
            '2' => (697.0, 1336.0),
            '3' => (697.0, 1477.0),
            '4' => (770.0, 1209.0),
            '5' => (770.0, 1336.0),
            '6' => (770.0, 1477.0),
            '7' => (852.0, 1209.0),
            '8' => (852.0, 1336.0),
            '9' => (852.0, 1477.0),
            '*' => (941.0, 1209.0),
            '0' => (941.0, 1336.0),
            '#' => (941.0, 1477.0),
            _ => panic!("unsupported digit"),
        };
        (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                0.5 * ((TAU * row * t).sin() + (TAU * col * t).sin())
            })
            .collect()
    }

    fn drain_pending(d: &mut DtmfDecoder) -> String {
        String::from_utf8(d.take_pending()).unwrap()
    }

    fn feed(d: &mut DtmfDecoder, audio: &[f32]) -> String {
        let mut out_buf = vec![0_u8; 4096];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(audio),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::Events(&mut out_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = d.process(&mut io).unwrap();
        String::from_utf8(out_buf[..w.produced[0]].to_vec()).unwrap()
    }

    #[test]
    fn silence_emits_nothing() {
        let mut d = DtmfDecoder::new(DtmfDecoderParams::default()).unwrap();
        let silence = vec![0.0_f32; 8_000];
        let out = feed(&mut d, &silence);
        assert!(out.is_empty());
        assert!(d.take_pending().is_empty());
    }

    #[test]
    fn single_digit_emits_one_event() {
        let fs = 8_000.0_f32;
        let p = DtmfDecoderParams {
            sample_rate_hz: fs,
            block_size: 200,
            hold_ms: 40,
            off_ms: 40,
            dominance: 4.0,
        };
        let mut d = DtmfDecoder::new(p).unwrap();
        // 200 ms tone = 1600 samples, plenty above hold_ms=40.
        let tone = gen_digit('5', 1600, fs);
        let _ = feed(&mut d, &tone);
        // Followed by silence → triggers release.
        let silence = vec![0.0_f32; 800];
        let out = feed(&mut d, &silence);
        let combined = out + &drain_pending(&mut d);
        assert!(combined.contains("\"digit\":\"5\""), "got: {combined}");
        // Exactly one event line.
        assert_eq!(combined.matches('\n').count(), 1, "got: {combined}");
    }

    #[test]
    fn four_digits_in_order() {
        let fs = 8_000.0_f32;
        let p = DtmfDecoderParams {
            sample_rate_hz: fs,
            block_size: 200,
            hold_ms: 40,
            off_ms: 40,
            dominance: 4.0,
        };
        let mut d = DtmfDecoder::new(p).unwrap();
        let mut combined = String::new();
        for c in ['1', '2', '3', '4'] {
            let tone = gen_digit(c, 1600, fs);
            combined.push_str(&feed(&mut d, &tone));
            let silence = vec![0.0_f32; 800];
            combined.push_str(&feed(&mut d, &silence));
        }
        combined.push_str(&drain_pending(&mut d));
        // Exactly 4 events, in order.
        let events: Vec<&str> = combined.lines().collect();
        assert_eq!(events.len(), 4, "got lines: {events:?}");
        for (expected, got) in ['1', '2', '3', '4'].iter().zip(events.iter()) {
            let needle = format!("\"digit\":\"{expected}\"");
            assert!(got.contains(&needle), "got: {got}");
        }
    }

    #[test]
    fn decodes_output_of_dtmf_audio_source() {
        // End-to-end block-pair test: DtmfAudioSource produces the audio,
        // DtmfDecoder recovers the digits. No synthetic gen_digit — the
        // real upstream block drives this.
        let fs = 8_000.0_f32;
        let sp = DtmfAudioSourceParams {
            rate_hz: fs,
            digits: "12".into(),
            hold_ms: 200,
            gap_ms: 80,
            amplitude: 1.0,
            loop_sequence: false,
        };
        let mut src = DtmfAudioSource::new(sp).unwrap();

        let mut buf = vec![0.0_f32; 4_096];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        let _ = src.process(&mut io).unwrap();

        let mut d = DtmfDecoder::new(DtmfDecoderParams {
            sample_rate_hz: fs,
            ..Default::default()
        })
        .unwrap();
        let out = feed(&mut d, &buf);
        let combined = out + &drain_pending(&mut d);
        assert!(combined.contains("\"digit\":\"1\""), "got: {combined}");
        assert!(combined.contains("\"digit\":\"2\""), "got: {combined}");
    }

    #[test]
    fn short_tone_below_hold_is_dropped() {
        let fs = 8_000.0_f32;
        let p = DtmfDecoderParams {
            sample_rate_hz: fs,
            block_size: 200,
            hold_ms: 100, // need 100 ms
            off_ms: 40,
            dominance: 4.0,
        };
        let mut d = DtmfDecoder::new(p).unwrap();
        // 30ms tone — too short.
        let tone = gen_digit('7', 240, fs);
        let _ = feed(&mut d, &tone);
        let silence = vec![0.0_f32; 800];
        let out = feed(&mut d, &silence);
        let combined = out + &drain_pending(&mut d);
        assert!(
            combined.is_empty(),
            "short tone should not emit: {combined}"
        );
    }
}
