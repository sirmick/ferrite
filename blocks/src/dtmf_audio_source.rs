//! DTMF audio source — programmatic keypad-tone generator.
//!
//! Plays a scripted sequence of DTMF digits as real-valued audio at
//! `rate_hz` (default 8 kHz, the DTMF spec rate). Each digit sums two
//! sine tones (one row + one column), held for `hold_ms` and separated
//! by `gap_ms` of silence. When the sequence ends, loops back to the
//! start unless `loop_sequence = false`.
//!
//! Used by the pre-Phase-1 events-bridge E2E test (`dtmf-e2e.json`) so
//! the test has a deterministic, dependency-free audio stream the DTMF
//! decoder can detect against.
//!
//! ### Phase accumulators
//!
//! Row and column tones each use a Q0.32 phase register advanced by an
//! integer increment per sample, matching [`crate::sine::SineSource`].
//! Integer wrapping is exact, so tone frequencies never drift across
//! long runs.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

const INV_PHASE_SCALE: f32 = 1.0_f32 / 4_294_967_296.0_f32;

/// `(row_hz, col_hz)` for a DTMF keypad character. Returns `None` for
/// characters that aren't on the keypad — the source inserts a silent
/// gap for those so callers can space digits with unrecognised chars.
const fn tone_pair(c: char) -> Option<(f32, f32)> {
    let row = match c {
        '1' | '2' | '3' | 'A' => 697.0,
        '4' | '5' | '6' | 'B' => 770.0,
        '7' | '8' | '9' | 'C' => 852.0,
        '*' | '0' | '#' | 'D' => 941.0,
        _ => return None,
    };
    let col = match c {
        '1' | '4' | '7' | '*' => 1209.0,
        '2' | '5' | '8' | '0' => 1336.0,
        '3' | '6' | '9' | '#' => 1477.0,
        'A' | 'B' | 'C' | 'D' => 1633.0,
        _ => return None,
    };
    Some((row, col))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DtmfAudioSourceParams {
    pub rate_hz: f32,
    pub digits: String,
    pub hold_ms: u32,
    pub gap_ms: u32,
    pub amplitude: f32,
    pub loop_sequence: bool,
}

impl Default for DtmfAudioSourceParams {
    fn default() -> Self {
        Self {
            rate_hz: 8_000.0,
            digits: "1234".into(),
            hold_ms: 200,
            gap_ms: 80,
            amplitude: 0.5,
            loop_sequence: true,
        }
    }
}

/// Whether the generator is currently emitting a digit or a gap.
#[derive(Debug, Clone, Copy)]
enum Phase {
    Tone {
        digit_idx: usize,
        samples_left: u64,
        row_phase: u32,
        col_phase: u32,
        row_inc: u32,
        col_inc: u32,
    },
    Gap {
        samples_left: u64,
    },
    Done,
}

pub struct DtmfAudioSource {
    params: DtmfAudioSourceParams,
    /// Only the keypad-recognisable characters of `params.digits`, in
    /// order. Unrecognised characters are dropped at construction.
    digits: Vec<char>,
    hold_samples: u64,
    gap_samples: u64,
    phase: Phase,
    /// Next digit index to play when the current `Gap` ends. Recorded
    /// when `Tone` transitions to `Gap` so the gap handler knows where
    /// to resume without threading it through `Phase::Gap`.
    next_digit_idx_after_gap: usize,
}

impl DtmfAudioSource {
    pub fn new(params: DtmfAudioSourceParams) -> Result<Self> {
        if !(params.rate_hz.is_finite() && params.rate_hz > 0.0) {
            bail!("dtmf_audio_source rate_hz must be > 0");
        }
        if !(params.amplitude.is_finite() && params.amplitude >= 0.0) {
            bail!("dtmf_audio_source amplitude must be >= 0");
        }
        let digits: Vec<char> = params
            .digits
            .chars()
            .filter(|c| tone_pair(*c).is_some())
            .collect();
        if digits.is_empty() {
            bail!(
                "dtmf_audio_source digits must contain at least one keypad char (got {:?})",
                params.digits
            );
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let hold_samples =
            ((f64::from(params.hold_ms) * f64::from(params.rate_hz)) / 1000.0).round() as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let gap_samples =
            ((f64::from(params.gap_ms) * f64::from(params.rate_hz)) / 1000.0).round() as u64;
        let mut s = Self {
            params,
            digits,
            hold_samples,
            gap_samples,
            phase: Phase::Gap { samples_left: 0 },
            next_digit_idx_after_gap: 0,
        };
        s.advance_from_gap(0);
        Ok(s)
    }

    fn phase_inc(&self, freq_hz: f32) -> u32 {
        let cycles_per_sample = freq_hz / self.params.rate_hz;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let scaled = (f64::from(cycles_per_sample) * f64::from(u32::MAX)
            + f64::from(cycles_per_sample)) as i64;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        {
            scaled as i32 as u32
        }
    }

    fn start_tone(&mut self, digit_idx: usize) {
        let c = self.digits[digit_idx];
        let (row, col) = tone_pair(c).expect("filtered at construction");
        self.phase = Phase::Tone {
            digit_idx,
            samples_left: self.hold_samples,
            row_phase: 0,
            col_phase: 0,
            row_inc: self.phase_inc(row),
            col_inc: self.phase_inc(col),
        };
    }

    /// Called whenever a `Gap` ends: either advance to the next digit,
    /// wrap to digit 0 if `loop_sequence`, or park in `Done`.
    fn advance_from_gap(&mut self, finished_digit_idx: usize) {
        let next = finished_digit_idx;
        if next < self.digits.len() {
            self.start_tone(next);
        } else if self.params.loop_sequence {
            self.start_tone(0);
        } else {
            self.phase = Phase::Done;
        }
    }

    /// Test hook — which digit character (if any) is currently being
    /// emitted.
    #[must_use]
    pub fn current_digit(&self) -> Option<char> {
        match self.phase {
            Phase::Tone { digit_idx, .. } => Some(self.digits[digit_idx]),
            _ => None,
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DtmfAudioSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DtmfAudioSource",
            placement: Placement::Either,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[
                ParamSpec {
                    key: "rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::EnumNumeric {
                        values: &[8_000.0, 16_000.0, 22_050.0, 48_000.0],
                        default: 8_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "digits",
                    label: "Digit sequence",
                    kind: ParamKind::Text { default: "1234" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "hold_ms",
                    label: "Digit duration",
                    kind: ParamKind::Range {
                        min: 20.0,
                        max: 2_000.0,
                        step: 1.0,
                        default: 200.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "gap_ms",
                    label: "Inter-digit gap",
                    kind: ParamKind::Range {
                        min: 20.0,
                        max: 2_000.0,
                        step: 1.0,
                        default: 80.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "amplitude",
                    label: "Amplitude",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.001,
                        default: 0.5,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "loop_sequence",
                    label: "Loop sequence",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(f64::from(self.params.rate_hz))
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        // Sum of two unit sines has peak 2.0 — halve before applying the
        // user amplitude so amplitude=1.0 stays inside [-1, 1].
        let half_amp = self.params.amplitude * 0.5;
        let mut produced = 0;
        while produced < dst.len() {
            match self.phase {
                Phase::Done => {
                    dst[produced] = 0.0;
                    produced += 1;
                }
                Phase::Gap { samples_left } => {
                    let take = (dst.len() - produced).min(samples_left as usize);
                    for s in &mut dst[produced..produced + take] {
                        *s = 0.0;
                    }
                    produced += take;
                    let remaining = samples_left - take as u64;
                    if remaining == 0 {
                        // Gap complete — figure out which digit finished
                        // and advance. finished_digit_idx is the one the
                        // most-recent Tone owned; we recorded that
                        // implicitly as "next digit to play" by seeding
                        // the gap from the end of Tone. See handoff below.
                        self.advance_from_gap(self.next_digit_idx_after_gap);
                    } else {
                        self.phase = Phase::Gap {
                            samples_left: remaining,
                        };
                    }
                }
                Phase::Tone {
                    digit_idx,
                    samples_left,
                    mut row_phase,
                    mut col_phase,
                    row_inc,
                    col_inc,
                } => {
                    let take = (dst.len() - produced).min(samples_left as usize);
                    for s in &mut dst[produced..produced + take] {
                        #[allow(clippy::cast_precision_loss)]
                        let theta_r = (row_phase as f32 * INV_PHASE_SCALE) * core::f32::consts::TAU;
                        #[allow(clippy::cast_precision_loss)]
                        let theta_c = (col_phase as f32 * INV_PHASE_SCALE) * core::f32::consts::TAU;
                        *s = half_amp * (theta_r.sin() + theta_c.sin());
                        row_phase = row_phase.wrapping_add(row_inc);
                        col_phase = col_phase.wrapping_add(col_inc);
                    }
                    produced += take;
                    let remaining = samples_left - take as u64;
                    if remaining == 0 {
                        // Tone ends — schedule its gap and remember which
                        // digit just finished so advance_from_gap picks the
                        // correct next index.
                        self.next_digit_idx_after_gap = digit_idx + 1;
                        self.phase = Phase::Gap {
                            samples_left: self.gap_samples,
                        };
                    } else {
                        self.phase = Phase::Tone {
                            digit_idx,
                            samples_left: remaining,
                            row_phase,
                            col_phase,
                            row_inc,
                            col_inc,
                        };
                    }
                }
            }
        }

        let mut w = Work::new();
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for DtmfAudioSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DtmfAudioSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DtmfAudioSource::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{DtmfAudioSource, DtmfAudioSourceParams};
    use crate::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};

    fn run(src: &mut DtmfAudioSource, n: usize) -> Vec<f32> {
        let mut buf = vec![0.0_f32; n];
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
        buf
    }

    #[test]
    fn rejects_empty_digits() {
        let p = DtmfAudioSourceParams {
            digits: String::new(),
            ..Default::default()
        };
        assert!(DtmfAudioSource::new(p).is_err());
    }

    #[test]
    fn drops_unknown_chars() {
        // " 1 2 " → only '1' and '2' survive.
        let p = DtmfAudioSourceParams {
            digits: " 1 2 ".into(),
            ..Default::default()
        };
        let src = DtmfAudioSource::new(p).unwrap();
        assert_eq!(src.digits, ['1', '2']);
    }

    #[test]
    fn starts_on_first_digit() {
        // After construction, phase is Tone(0) — we artificially set an
        // initial Gap(0) in `new` and immediately advance, so the first
        // process() call emits digit 0's tones.
        let p = DtmfAudioSourceParams {
            digits: "1".into(),
            rate_hz: 8_000.0,
            hold_ms: 10,
            gap_ms: 10,
            amplitude: 1.0,
            loop_sequence: false,
        };
        let mut src = DtmfAudioSource::new(p).unwrap();
        assert_eq!(src.current_digit(), Some('1'));
        let out = run(&mut src, 40);
        // Non-zero somewhere in the first 80 samples.
        assert!(out.iter().any(|s| s.abs() > 0.1));
    }

    #[test]
    fn emits_silence_in_gap() {
        // hold=0 is rejected by the range but we simulate it by using a
        // small hold + inspecting the gap segment.
        let p = DtmfAudioSourceParams {
            digits: "12".into(),
            rate_hz: 8_000.0,
            hold_ms: 5,
            gap_ms: 10,
            amplitude: 1.0,
            loop_sequence: false,
        };
        let mut src = DtmfAudioSource::new(p).unwrap();
        // 5ms × 8kHz = 40 samples of tone, then 10ms × 8kHz = 80 of gap.
        let out = run(&mut src, 40 + 80);
        let gap = &out[40..40 + 80];
        for s in gap {
            assert_eq!(*s, 0.0, "gap must be silent");
        }
    }

    #[test]
    fn loops_by_default() {
        // Single digit, looping — stream never ends.
        let p = DtmfAudioSourceParams {
            digits: "5".into(),
            rate_hz: 8_000.0,
            hold_ms: 5,
            gap_ms: 5,
            amplitude: 1.0,
            loop_sequence: true,
        };
        let mut src = DtmfAudioSource::new(p).unwrap();
        // Run several cycles — no zero-output tail, tone periodically
        // returns after each gap.
        let n = 8_000 / 100; // 10ms
        let mut saw_nonzero_late = false;
        for _ in 0..10 {
            let out = run(&mut src, n);
            if out.iter().any(|s| s.abs() > 0.1) {
                saw_nonzero_late = true;
            }
        }
        assert!(saw_nonzero_late, "loop_sequence=true never re-enters tone");
    }

    #[test]
    fn stops_when_not_looping() {
        let p = DtmfAudioSourceParams {
            digits: "7".into(),
            rate_hz: 8_000.0,
            hold_ms: 2,
            gap_ms: 2,
            amplitude: 1.0,
            loop_sequence: false,
        };
        let mut src = DtmfAudioSource::new(p).unwrap();
        // 2ms × 8kHz = 16 tone + 16 gap = 32 samples before Done.
        let out = run(&mut src, 128);
        // Last 64 should be strictly zero.
        for s in &out[64..] {
            assert_eq!(*s, 0.0);
        }
    }
}
