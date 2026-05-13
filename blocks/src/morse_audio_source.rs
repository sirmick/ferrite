//! Morse audio source — text-to-CW generator for first-principles E2E
//! tests of the Morse decode chain.
//!
//! Plays a scripted text string as on-off-keyed tone audio at `rate_hz`
//! (default 22050 Hz, multimon's native rate). Tone frequency, WPM, and
//! amplitude are runtime params. Pair with [`crate::morse::MorseDemod`]
//! to round-trip text → audio → decoded text without an SDR or off-air
//! recording.
//!
//! ### Timing
//!
//! ITU-standard CW: a "unit" is `1200 / wpm` ms (PARIS-50 convention).
//! A dit is 1 unit ON, a dah is 3 units ON. Intra-character gap is
//! 1 unit OFF (between symbols of the same letter), inter-character
//! gap is 3 units (between letters of the same word), inter-word gap
//! is 7 units (between space-separated words).
//!
//! ### Encoding
//!
//! Looks up each ASCII character in [`MORSE`]; unknown characters are
//! treated as inter-word gaps so a stream like `"HI 73"` plays cleanly.
//! Lowercase is folded to uppercase before lookup.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

const INV_PHASE_SCALE: f32 = 1.0_f32 / 4_294_967_296.0_f32;

/// Subset of the ITU Morse table — letters, digits, and a couple of
/// common punctuation characters that turn up in pre-canned test
/// strings (`HELLO WORLD`, `CQ DE`, `73`, `?`).
const MORSE: &[(char, &str)] = &[
    ('A', ".-"),
    ('B', "-..."),
    ('C', "-.-."),
    ('D', "-.."),
    ('E', "."),
    ('F', "..-."),
    ('G', "--."),
    ('H', "...."),
    ('I', ".."),
    ('J', ".---"),
    ('K', "-.-"),
    ('L', ".-.."),
    ('M', "--"),
    ('N', "-."),
    ('O', "---"),
    ('P', ".--."),
    ('Q', "--.-"),
    ('R', ".-."),
    ('S', "..."),
    ('T', "-"),
    ('U', "..-"),
    ('V', "...-"),
    ('W', ".--"),
    ('X', "-..-"),
    ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"),
    ('1', ".----"),
    ('2', "..---"),
    ('3', "...--"),
    ('4', "....-"),
    ('5', "....."),
    ('6', "-...."),
    ('7', "--..."),
    ('8', "---.."),
    ('9', "----."),
    ('.', ".-.-.-"),
    (',', "--..--"),
    ('?', "..--.."),
    ('/', "-..-."),
];

fn morse_for(c: char) -> Option<&'static str> {
    let upper = c.to_ascii_uppercase();
    MORSE.iter().find(|(k, _)| *k == upper).map(|(_, v)| *v)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MorseAudioSourceParams {
    pub rate_hz: f32,
    pub text: String,
    pub wpm: u32,
    pub tone_hz: f32,
    pub amplitude: f32,
    pub loop_sequence: bool,
}

impl Default for MorseAudioSourceParams {
    fn default() -> Self {
        Self {
            rate_hz: 22_050.0,
            text: "PARIS PARIS PARIS".into(),
            wpm: 20,
            tone_hz: 700.0,
            amplitude: 0.5,
            loop_sequence: true,
        }
    }
}

/// One entry in the playback schedule: a span of samples either fully
/// keyed (tone on) or fully silent. The schedule is built once at
/// construction from `params.text` and then walked sample-by-sample
/// in `process()`.
#[derive(Debug, Clone, Copy)]
struct Slot {
    on: bool,
    samples: u64,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Currently emitting `slots[idx]`. `samples_left` counts down to
    /// the next slot transition.
    Slot {
        idx: usize,
        samples_left: u64,
    },
    Done,
}

pub struct MorseAudioSource {
    params: MorseAudioSourceParams,
    slots: Vec<Slot>,
    phase_inc: u32,
    phase: u32,
    state: Phase,
}

impl MorseAudioSource {
    pub fn new(params: MorseAudioSourceParams) -> Result<Self> {
        if !(params.rate_hz.is_finite() && params.rate_hz > 0.0) {
            bail!("morse_audio_source rate_hz must be > 0");
        }
        if !(params.tone_hz.is_finite() && params.tone_hz > 0.0) {
            bail!("morse_audio_source tone_hz must be > 0");
        }
        if params.wpm == 0 {
            bail!("morse_audio_source wpm must be > 0");
        }
        if !(params.amplitude.is_finite() && params.amplitude >= 0.0) {
            bail!("morse_audio_source amplitude must be >= 0");
        }
        if params.text.is_empty() {
            bail!("morse_audio_source text must not be empty");
        }

        let unit_ms = 1200.0_f64 / f64::from(params.wpm);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let unit_samples = ((unit_ms * f64::from(params.rate_hz)) / 1000.0).round() as u64;
        if unit_samples == 0 {
            bail!(
                "morse_audio_source: unit interval rounded to 0 samples (rate={} wpm={})",
                params.rate_hz,
                params.wpm
            );
        }

        let slots = build_schedule(&params.text, unit_samples);
        let cycles_per_sample = params.tone_hz / params.rate_hz;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let phase_inc = (f64::from(cycles_per_sample) * f64::from(u32::MAX)
            + f64::from(cycles_per_sample)) as i64 as u32;

        let initial_phase = if slots.is_empty() {
            Phase::Done
        } else {
            Phase::Slot {
                idx: 0,
                samples_left: slots[0].samples,
            }
        };

        Ok(Self {
            params,
            slots,
            phase_inc,
            phase: 0,
            state: initial_phase,
        })
    }

    fn advance_slot(&mut self) {
        let (cur_idx, _) = match self.state {
            Phase::Slot { idx, .. } => (idx, ()),
            Phase::Done => return,
        };
        let next_idx = cur_idx + 1;
        if next_idx < self.slots.len() {
            self.state = Phase::Slot {
                idx: next_idx,
                samples_left: self.slots[next_idx].samples,
            };
        } else if self.params.loop_sequence {
            self.state = Phase::Slot {
                idx: 0,
                samples_left: self.slots[0].samples,
            };
            // Reset the carrier phase between loops so the first symbol
            // of each repetition starts at the same point — keeps the
            // emit deterministic for fixture-based tests.
            self.phase = 0;
        } else {
            self.state = Phase::Done;
        }
    }
}

/// Translate `text` into a schedule of (on/off, samples) slots. Adjacent
/// slots of the same on/off state are merged so the runtime walker
/// doesn't see redundant transitions.
fn build_schedule(text: &str, unit_samples: u64) -> Vec<Slot> {
    let mut out: Vec<Slot> = Vec::new();
    let mut push = |on: bool, units: u64| {
        if units == 0 {
            return;
        }
        let n = unit_samples.saturating_mul(units);
        if let Some(last) = out.last_mut() {
            if last.on == on {
                last.samples = last.samples.saturating_add(n);
                return;
            }
        }
        out.push(Slot { on, samples: n });
    };

    let mut prev_was_letter = false;
    for ch in text.chars() {
        let pat = morse_for(ch);
        if pat.is_none() {
            // Unknown char → treat as a word break.
            // Existing trailing inter-character gap (3 units) gets
            // extended to inter-word (7 units) by the merge logic.
            if prev_was_letter {
                push(false, 7 - 3);
            } else {
                push(false, 7);
            }
            prev_was_letter = false;
            continue;
        }
        let pat = pat.unwrap();
        if prev_was_letter {
            // Already emitted 1 unit OFF after the previous letter's
            // last symbol; add 2 more to reach the inter-character gap
            // total of 3 units.
            push(false, 2);
        }
        let mut first_symbol = true;
        for sym in pat.chars() {
            if !first_symbol {
                push(false, 1); // intra-character gap
            }
            match sym {
                '.' => push(true, 1),
                '-' => push(true, 3),
                _ => continue,
            }
            first_symbol = false;
        }
        // Trailing 1-unit gap so the next iteration (whether letter or
        // word break) can extend cleanly.
        push(false, 1);
        prev_was_letter = true;
    }
    out
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for MorseAudioSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "MorseAudioSource",
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
                        default: 22_050.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Output audio rate.",
                },
                ParamSpec {
                    key: "text",
                    label: "Text to send",
                    kind: ParamKind::Text {
                        default: "PARIS PARIS PARIS",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Plain text to key. Letters, digits, basic punctuation; unknown chars become word breaks.",
                },
                ParamSpec {
                    key: "wpm",
                    label: "Words per minute",
                    kind: ParamKind::Range {
                        min: 5.0,
                        max: 60.0,
                        step: 1.0,
                        default: 20.0,
                        unit: "wpm",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Words per minute (PARIS standard). 12–25 wpm is comfortable conversational CW.",
                },
                ParamSpec {
                    key: "tone_hz",
                    label: "Tone frequency",
                    kind: ParamKind::Range {
                        min: 200.0,
                        max: 2_000.0,
                        step: 1.0,
                        default: 700.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Carrier pitch. 600–800 Hz is the traditional CW comfort range.",
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
                    ai_notes: "Output level, 0–1.",
                },
                ParamSpec {
                    key: "loop_sequence",
                    label: "Loop sequence",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "True = restart text from start on EOF. False = silence after.",
                },
            ],
            ai_notes: "Audio-rate CW tone generator that keys a given text in Morse. Pair with `MorseDemod` for end-to-end tests of the decode chain without an antenna.",
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

        let amp = self.params.amplitude;
        let inc = self.phase_inc;
        let mut produced = 0;
        while produced < dst.len() {
            match self.state {
                Phase::Done => {
                    dst[produced] = 0.0;
                    produced += 1;
                }
                Phase::Slot { idx, samples_left } => {
                    let on = self.slots[idx].on;
                    let take = (dst.len() - produced).min(samples_left as usize);
                    if on {
                        for s in &mut dst[produced..produced + take] {
                            #[allow(clippy::cast_precision_loss)]
                            let theta =
                                (self.phase as f32 * INV_PHASE_SCALE) * core::f32::consts::TAU;
                            *s = amp * theta.sin();
                            self.phase = self.phase.wrapping_add(inc);
                        }
                    } else {
                        for s in &mut dst[produced..produced + take] {
                            *s = 0.0;
                        }
                    }
                    produced += take;
                    let remaining = samples_left - take as u64;
                    if remaining == 0 {
                        self.advance_slot();
                    } else {
                        self.state = Phase::Slot {
                            idx,
                            samples_left: remaining,
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

impl BlockFactory for MorseAudioSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: MorseAudioSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(MorseAudioSource::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{build_schedule, MorseAudioSource, MorseAudioSourceParams};
    use crate::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};

    fn run(src: &mut MorseAudioSource, n: usize) -> Vec<f32> {
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
    fn rejects_empty_text() {
        let p = MorseAudioSourceParams {
            text: String::new(),
            ..Default::default()
        };
        assert!(MorseAudioSource::new(p).is_err());
    }

    #[test]
    fn rejects_zero_wpm() {
        let p = MorseAudioSourceParams {
            wpm: 0,
            ..Default::default()
        };
        assert!(MorseAudioSource::new(p).is_err());
    }

    #[test]
    fn schedule_for_e_is_one_dit() {
        // 'E' = single dit: ON(1) + trailing OFF(1).
        let s = build_schedule("E", 100);
        assert_eq!(s.len(), 2);
        assert!(s[0].on && s[0].samples == 100);
        assert!(!s[1].on && s[1].samples == 100);
    }

    #[test]
    fn schedule_for_paris_is_close_to_44_units() {
        // ITU PARIS is 43 units of in-text time (dits, dahs, intra-
        // and inter-letter gaps) + 7 units of trailing inter-word gap
        // = 50 per word at the rated WPM. Our schedule emits a trailing
        // 1-unit gap per letter (4 inter-letter extensions of +2 plus
        // 5 trailing 1-unit gaps) which sums to 44 — one unit slower
        // than ITU. Acceptable for the decoder; revisit if a real
        // WPM-calibrated test ever needs sub-percent accuracy.
        let unit = 100_u64;
        let s = build_schedule("PARIS", unit);
        let total: u64 = s.iter().map(|sl| sl.samples).sum();
        assert_eq!(total, 44 * unit);
    }

    #[test]
    fn produces_audio_then_silence_when_not_looping() {
        let p = MorseAudioSourceParams {
            text: "E".into(),
            rate_hz: 22_050.0,
            wpm: 20,
            tone_hz: 700.0,
            amplitude: 1.0,
            loop_sequence: false,
        };
        let mut src = MorseAudioSource::new(p).unwrap();
        // 1 unit at 20 wpm = 60 ms = 1323 samples; ON then OFF then Done.
        let out = run(&mut src, 22_050 / 4); // ~250 ms
                                             // First ~1323 samples: tone, non-zero somewhere.
        assert!(out[..1000].iter().any(|s| s.abs() > 0.5));
        // Tail: silent (Done state).
        for s in &out[5000..] {
            assert_eq!(*s, 0.0);
        }
    }

    #[test]
    fn loops_when_loop_sequence_set() {
        let p = MorseAudioSourceParams {
            text: "E".into(),
            rate_hz: 22_050.0,
            wpm: 30,
            tone_hz: 700.0,
            amplitude: 1.0,
            loop_sequence: true,
        };
        let mut src = MorseAudioSource::new(p).unwrap();
        // Run several units worth — should see at least two tone bursts.
        let out = run(&mut src, 22_050); // 1 second
                                         // Count zero-crossings of envelope: rough proxy for burst count.
        let mut bursts = 0;
        let mut in_tone = false;
        for window in out.chunks(200) {
            let energy: f32 = window.iter().map(|s| s.abs()).sum::<f32>() / window.len() as f32;
            let now_tone = energy > 0.1;
            if now_tone && !in_tone {
                bursts += 1;
            }
            in_tone = now_tone;
        }
        assert!(
            bursts >= 2,
            "loop_sequence=true should produce ≥2 bursts in 1 s, got {bursts}"
        );
    }
}
