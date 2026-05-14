//! Unified audio noise-reduction blocks.
//!
//! Two block wrappers — [`AudioNrMono`] and [`AudioNrStereo`] — share a
//! single stack of DSP stages, toggled per-stage by the
//! [`AudioNrParams`] struct. Presets set band-appropriate defaults
//! (FM-broadcast vs. HF-SSB vs. digital-mode); the block never cares
//! which preset it's running in.
//!
//! ### Stage order
//!
//! ```text
//! input → deemph → blanker → notch → spectral → neural → agc → output
//! ```
//!
//! The ordering is load-bearing. Pre-emphasis inversion first
//! normalises the spectral tilt that broadcast FM introduces, so
//! downstream estimators see an untilted reference. Impulse blanking
//! runs *before* the spectral noise-tracker so big transients don't
//! inflate the learned noise floor. Adaptive notch runs before
//! spectral subtraction so heterodynes aren't smeared across bins by
//! the FFT. Neural sits before AGC so the gain stage adapts to
//! denoised levels rather than raw noisy ones. AGC is last —
//! whatever the upstream stages do, the output stays at a predictable
//! loudness regardless of input amplitude, which matters most for AM
//! broadcast where station strengths span 30+ dB.
//!
//! ### Mono vs. stereo
//!
//! - [`AudioNrMono`]: one input, one output, every stage runs on the
//!   single channel.
//! - [`AudioNrStereo`]: two inputs (`left`, `right`), two outputs
//!   (`left`, `right`). Deemph, blanker, notch run per-channel
//!   independently (linear and cheap; per-channel converge is fine).
//!   Once spectral and neural stages carry gain-envelope outputs they
//!   will switch to **shared-gain** mode — detect on `M=(L+R)/2`,
//!   emit a single gain, apply the same gain to both L and R so the
//!   stereo image is preserved exactly. The stub implementations
//!   run per-channel as a passthrough and will flip to shared-gain
//!   when the real stages land in tasks #37 and #38.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

pub mod agc;
pub mod blanker;
pub mod deemph;
pub mod neural;
pub mod notch;
pub mod spectral;

pub use spectral::SpectralMethod;

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AudioNrParams {
    /// Declared input sample rate — construction-time hint; `init()`
    /// and `update_rates()` read the scheduler's live value and
    /// recompute rate-dependent coefficients.
    pub sample_rate_hz: f32,

    // -- de-emphasis ------------------------------------------------
    pub deemph_enable: bool,
    /// 75 µs (US) or 50 µs (EU). Ignored when `deemph_enable = false`.
    pub deemph_tau_us: f32,

    // -- impulse blanker -------------------------------------------
    pub blanker_enable: bool,
    /// Spike threshold in dB above the slow envelope.
    pub blanker_threshold_db: f32,
    /// How long to hold blanking after a trigger.
    pub blanker_hold_ms: f32,

    // -- adaptive LMS notch ----------------------------------------
    pub notch_enable: bool,
    pub notch_taps: u32,
    pub notch_mu: f32,
    pub notch_delay: u32,

    // -- spectral subtraction --------------------------------------
    pub spectral_enable: bool,
    pub spectral_method: SpectralMethod,
    pub spectral_block_size: usize,
    pub spectral_oversub: f32,
    pub spectral_floor: f32,
    pub spectral_noise_alpha: f32,

    // -- neural (DFN3) ---------------------------------------------
    pub neural_enable: bool,
    /// Maximum suppression depth (dB). Lets the user cap how much
    /// the neural stage attenuates, for a more natural sound.
    pub neural_attenuation_db: f32,

    // -- AGC --------------------------------------------------------
    pub agc_enable: bool,
    /// Target peak output level in dBFS (relative to ±1.0). The AGC
    /// adapts gain to keep the running envelope near this. −6 dBFS
    /// leaves 6 dB of headroom before the AudioWorklet clips on
    /// the brief transient before the peak-detector update fires.
    pub agc_target_dbfs: f32,
    /// Envelope release time-constant. The attack is instantaneous
    /// (peak hold) — larger release = no pumping on speech pauses,
    /// smaller = faster recovery on dynamic music.
    pub agc_release_ms: f32,
    /// Maximum upward gain (dB) on quiet signals. Caps how much the
    /// AGC will amplify silence/noise during transmission gaps.
    pub agc_max_gain_db: f32,
}

impl Default for AudioNrParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,

            deemph_enable: false,
            deemph_tau_us: 75.0,

            blanker_enable: false,
            blanker_threshold_db: 20.0,
            blanker_hold_ms: 0.5,

            notch_enable: false,
            notch_taps: 64,
            notch_mu: 0.01,
            notch_delay: 1,

            spectral_enable: false,
            spectral_method: SpectralMethod::Boll,
            spectral_block_size: 512,
            spectral_oversub: 1.5,
            spectral_floor: 0.1,
            spectral_noise_alpha: 0.05,

            neural_enable: false,
            neural_attenuation_db: 18.0,

            agc_enable: false,
            agc_target_dbfs: -6.0,
            agc_release_ms: 500.0,
            agc_max_gain_db: 20.0,
        }
    }
}

impl AudioNrParams {
    fn validate(&self) -> Result<()> {
        if !(self.sample_rate_hz.is_finite() && self.sample_rate_hz > 0.0) {
            bail!("audio_nr: sample_rate_hz must be > 0");
        }
        if self.deemph_enable && !(self.deemph_tau_us.is_finite() && self.deemph_tau_us > 0.0) {
            bail!("audio_nr: deemph_tau_us must be > 0 when deemph is enabled");
        }
        if self.blanker_enable && !(self.blanker_hold_ms.is_finite() && self.blanker_hold_ms > 0.0)
        {
            bail!("audio_nr: blanker_hold_ms must be > 0 when blanker is enabled");
        }
        if self.notch_enable {
            if self.notch_taps < 2 {
                bail!("audio_nr: notch_taps must be ≥ 2");
            }
            if !(self.notch_mu.is_finite() && self.notch_mu > 0.0) {
                bail!("audio_nr: notch_mu must be > 0");
            }
            if self.notch_delay < 1 {
                bail!("audio_nr: notch_delay must be ≥ 1");
            }
        }
        if self.agc_enable {
            if !(self.agc_target_dbfs.is_finite() && self.agc_target_dbfs <= 0.0) {
                bail!("audio_nr: agc_target_dbfs must be ≤ 0 dBFS");
            }
            if !(self.agc_release_ms.is_finite() && self.agc_release_ms > 0.0) {
                bail!("audio_nr: agc_release_ms must be > 0");
            }
            if !(self.agc_max_gain_db.is_finite() && self.agc_max_gain_db >= 0.0) {
                bail!("audio_nr: agc_max_gain_db must be ≥ 0");
            }
        }
        if self.spectral_enable {
            let bs = self.spectral_block_size;
            if !(bs >= 64 && bs.is_power_of_two()) {
                bail!("audio_nr: spectral_block_size must be power-of-two and ≥ 64");
            }
            if !(self.spectral_oversub.is_finite() && self.spectral_oversub > 0.0) {
                bail!("audio_nr: spectral_oversub must be > 0");
            }
            if !(self.spectral_floor.is_finite() && (0.0..=1.0).contains(&self.spectral_floor)) {
                bail!("audio_nr: spectral_floor must be in [0, 1]");
            }
            if !(self.spectral_noise_alpha.is_finite()
                && (0.0..=1.0).contains(&self.spectral_noise_alpha))
            {
                bail!("audio_nr: spectral_noise_alpha must be in [0, 1]");
            }
        }
        Ok(())
    }
}

// Static parameter metadata for both Mono and Stereo block specs.
// `&'static` so it can live in the const `BlockSpec` returned by
// `spec()`. Keep this list synchronised with `AudioNrParams`.
const COMMON_PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "sample_rate_hz",
        label: "Input sample rate",
        kind: ParamKind::Range {
            min: 1_000.0,
            max: 1_000_000.0,
            step: 1.0,
            default: 48_000.0,
            unit: "Hz",
        },
        reconfig_scope: ReconfigureScope::SourceRestart,
        ai_notes: "Audio rate (typically 48 kHz). Set by the upstream resampler.",
    },
    ParamSpec {
        key: "deemph_enable",
        label: "FM de-emphasis",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 1 of the NR chain. Inverse of FM-broadcast pre-emphasis. Enable for WBFM/wbfm_stereo only; harmful on non-FM modes (no pre-emphasis to undo).",
    },
    ParamSpec {
        key: "deemph_tau_us",
        label: "De-emphasis τ",
        kind: ParamKind::EnumNumeric {
            values: &[50.0, 75.0],
            default: 75.0,
            unit: "µs",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Time constant — 75 µs in US/JP, 50 µs in EU/AU. Match the broadcaster's region.",
    },
    ParamSpec {
        key: "blanker_enable",
        label: "Impulse blanker",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 2. Wipes brief loud transients (lightning, ignition pops, electrical clicks). Always-on for HF AM/SSB; selective elsewhere.",
    },
    ParamSpec {
        key: "blanker_threshold_db",
        label: "Blanker threshold",
        kind: ParamKind::Range {
            min: 6.0,
            max: 40.0,
            step: 1.0,
            default: 20.0,
            unit: "dB",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Transients louder than the slow envelope by this many dB get blanked. 12–20 dB useful range; too low blanks voice peaks.",
    },
    ParamSpec {
        key: "blanker_hold_ms",
        label: "Blanker hold",
        kind: ParamKind::Range {
            min: 0.1,
            max: 5.0,
            step: 0.1,
            default: 0.5,
            unit: "ms",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "How long to hold the blank once triggered. 0.3–1.0 ms covers typical impulse widths.",
    },
    ParamSpec {
        key: "notch_enable",
        label: "Auto-notch",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 3. Adaptive LMS notch for tonal interference (heterodynes, AC hum + harmonics, fixed carriers). Turn on when you hear a fixed whistle.",
    },
    ParamSpec {
        key: "notch_taps",
        label: "Notch filter length",
        kind: ParamKind::EnumNumeric {
            values: &[32.0, 64.0, 128.0],
            default: 64.0,
            unit: "taps",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "FIR length. 64 is a good default; 128 for narrower notches at the cost of more CPU.",
    },
    ParamSpec {
        key: "notch_mu",
        label: "Notch adaptation rate",
        kind: ParamKind::Range {
            min: 0.0001,
            max: 0.1,
            step: 0.0001,
            default: 0.01,
            unit: "",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "LMS step size. 0.005–0.02 useful range; higher converges faster but tracks voice into the notch, killing consonants.",
    },
    ParamSpec {
        key: "notch_delay",
        label: "Notch decorrelation delay",
        kind: ParamKind::Range {
            min: 1.0,
            max: 16.0,
            step: 1.0,
            default: 1.0,
            unit: "samples",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Samples of delay between the notch's input and its reference path. 1–2 decorrelates voice from the notch so speech survives.",
    },
    ParamSpec {
        key: "spectral_enable",
        label: "Spectral NR",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 4. FFT-domain noise-floor reduction against an estimated stationary noise model. Best for steady noise (atmospheric hiss, hum); poor against bursty interference.",
    },
    ParamSpec {
        key: "spectral_method",
        label: "Spectral method",
        kind: ParamKind::EnumString {
            values: &["boll", "mmse_lsa"],
            default: "boll",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "`boll` = classic spectral subtraction (faster, more \"musical\" residue). `mmse_lsa` = log-spectral amplitude (better-perceived for voice, slightly more CPU).",
    },
    ParamSpec {
        key: "spectral_block_size",
        label: "Spectral FFT block size",
        kind: ParamKind::EnumNumeric {
            values: &[128.0, 256.0, 512.0, 1024.0, 2048.0],
            default: 512.0,
            unit: "samples",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "FFT window length. Larger = finer frequency resolution + more latency. 512 at 48 kHz ≈ 10 ms.",
    },
    ParamSpec {
        key: "spectral_oversub",
        label: "Spectral over-subtraction β",
        kind: ParamKind::Range {
            min: 0.5,
            max: 3.0,
            step: 0.1,
            default: 1.5,
            unit: "",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "How aggressively to over-subtract the estimated noise. 1.0–2.5 useful; higher = more aggressive, more musical artifacts.",
    },
    ParamSpec {
        key: "spectral_floor",
        label: "Spectral gain floor",
        kind: ParamKind::Range {
            min: 0.01,
            max: 1.0,
            step: 0.01,
            default: 0.1,
            unit: "",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Minimum residual gain per bin. Lower is quieter background but bubbles more (\"musical noise\"). 0.05–0.2 typical.",
    },
    ParamSpec {
        key: "spectral_noise_alpha",
        label: "Spectral noise learning α",
        kind: ParamKind::Range {
            min: 0.0,
            max: 1.0,
            step: 0.001,
            default: 0.05,
            unit: "",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "How fast the noise model adapts during silent gaps. Higher = faster tracking of changing noise; lower = stabler suppression on intermittent signals.",
    },
    ParamSpec {
        key: "neural_enable",
        label: "Neural NR (DFN3)",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 5 (final). DeepFilterNet 3 learned denoiser. Always-on default for voice modes; consider off for broadcast FM music (high-fidelity).",
    },
    ParamSpec {
        key: "neural_attenuation_db",
        label: "Neural max attenuation",
        kind: ParamKind::Range {
            min: 6.0,
            max: 60.0,
            step: 1.0,
            default: 18.0,
            unit: "dB",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Cap on suppression depth. 12 = subtle, 18 = default, 24 = strong (HF SSB), 30+ = aggressive (artifacts on music).",
    },
    ParamSpec {
        key: "agc_enable",
        label: "Audio AGC",
        kind: ParamKind::Toggle { default: false },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Stage 6 (final). Adapts gain so output peaks track a target level — keeps loud AM stations from clipping and weak DX from being inaudible. Always-on default on wbam; useful on any voice mode where station strengths vary widely.",
    },
    ParamSpec {
        key: "agc_target_dbfs",
        label: "AGC target",
        kind: ParamKind::Range {
            min: -24.0,
            max: 0.0,
            step: 0.5,
            default: -6.0,
            unit: "dBFS",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Peak output level the AGC tracks toward. −6 dBFS leaves 6 dB of headroom before the AudioWorklet clips on transients. Lower = quieter overall, more headroom.",
    },
    ParamSpec {
        key: "agc_release_ms",
        label: "AGC release τ",
        kind: ParamKind::Range {
            min: 50.0,
            max: 2000.0,
            step: 10.0,
            default: 500.0,
            unit: "ms",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "How fast the AGC raises gain when the input gets quieter. 300–800 ms avoids pumping on speech pauses; faster brings up the noise floor between syllables.",
    },
    ParamSpec {
        key: "agc_max_gain_db",
        label: "AGC max gain",
        kind: ParamKind::Range {
            min: 0.0,
            max: 60.0,
            step: 1.0,
            default: 20.0,
            unit: "dB",
        },
        reconfig_scope: ReconfigureScope::SelfBlock,
        ai_notes: "Upper bound on the AGC's boost. 20 dB covers strong-vs-DX on AM broadcast; higher amplifies background hiss when the channel is empty.",
    },
];

// ---------------------------------------------------------------------------
// Per-channel stage bank
// ---------------------------------------------------------------------------

/// All stages needed for one audio channel. Mono uses one; stereo uses
/// two. Each stage decides internally whether it's bypassed based on
/// the corresponding `*_enable` flag.
struct Chain {
    deemph: deemph::DeempStage,
    blanker: blanker::BlankerStage,
    notch: Option<notch::NotchStage>,
    spectral: spectral::SpectralStage,
    neural: neural::NeuralStage,
    agc: agc::AgcStage,
}

impl Chain {
    fn new(p: &AudioNrParams) -> Result<Self> {
        let rate = f64::from(p.sample_rate_hz);
        let deemph = deemph::DeempStage::new(p.deemph_tau_us, rate);
        let blanker = blanker::BlankerStage::new(p.blanker_threshold_db, p.blanker_hold_ms, rate);
        let notch = if p.notch_enable {
            Some(
                notch::NotchStage::new(p.notch_taps, p.notch_mu, p.notch_delay)
                    .map_err(anyhow::Error::msg)?,
            )
        } else {
            None
        };
        let spectral = spectral::SpectralStage::new(
            p.spectral_method,
            p.spectral_block_size,
            p.spectral_oversub,
            p.spectral_floor,
            p.spectral_noise_alpha,
        );
        let neural = neural::NeuralStage::new(p.neural_attenuation_db, rate);
        let agc = agc::AgcStage::new(p.agc_target_dbfs, p.agc_release_ms, p.agc_max_gain_db, rate);
        Ok(Self {
            deemph,
            blanker,
            notch,
            spectral,
            neural,
            agc,
        })
    }

    fn refresh_rate(&mut self, p: &AudioNrParams, rate_hz: f64) {
        self.deemph.recompute(p.deemph_tau_us, rate_hz);
        self.blanker
            .recompute(p.blanker_threshold_db, p.blanker_hold_ms, rate_hz);
        self.neural.set_input_rate(rate_hz);
        self.agc.recompute(
            p.agc_target_dbfs,
            p.agc_release_ms,
            p.agc_max_gain_db,
            rate_hz,
        );
    }

    fn run(&mut self, p: &AudioNrParams, buf: &mut [f32]) {
        if p.deemph_enable {
            self.deemph.run(buf);
        }
        if p.blanker_enable {
            self.blanker.run(buf);
        }
        if let Some(n) = self.notch.as_mut() {
            if p.notch_enable {
                n.run(buf);
            }
        }
        if p.spectral_enable {
            self.spectral.run(buf);
        }
        if p.neural_enable {
            self.neural.run(buf);
        }
        if p.agc_enable {
            self.agc.run(buf);
        }
    }
}

// ---------------------------------------------------------------------------
// Mono block
// ---------------------------------------------------------------------------

pub struct AudioNrMono {
    params: AudioNrParams,
    chain: Chain,
}

impl AudioNrMono {
    pub fn new(params: AudioNrParams) -> Result<Self> {
        params.validate()?;
        let chain = Chain::new(&params)?;
        Ok(Self { params, chain })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AudioNrMono {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AudioNrMono",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: COMMON_PARAMS,
            ai_notes: "Mono audio noise-reduction chain: deemph → blanker → notch → spectral → neural → agc, each independently toggleable. All params are live-tunable. Default profiles per mode are wired by the preset; tweak from `wbam` baseline (full stack + neural@18dB + agc@−6dBFS) or disable stages individually.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.chain.refresh_rate(&self.params, rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.chain.refresh_rate(&self.params, rate);
            }
        }
        Ok(())
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
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        self.chain.run(&self.params, &mut dst[..n]);

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for AudioNrMono {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AudioNrParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AudioNrMono::new(p)?))
    }
}

// ---------------------------------------------------------------------------
// Stereo block
// ---------------------------------------------------------------------------

pub struct AudioNrStereo {
    params: AudioNrParams,
    left: Chain,
    right: Chain,
}

impl AudioNrStereo {
    pub fn new(params: AudioNrParams) -> Result<Self> {
        params.validate()?;
        let left = Chain::new(&params)?;
        let right = Chain::new(&params)?;
        Ok(Self {
            params,
            left,
            right,
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AudioNrStereo {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AudioNrStereo",
            placement: Placement::Either,
            inputs: &[
                PortSpec {
                    name: "left",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "right",
                    port_type: PortType::RealF32,
                },
            ],
            outputs: &[
                PortSpec {
                    name: "left",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "right",
                    port_type: PortType::RealF32,
                },
            ],
            params: COMMON_PARAMS,
            ai_notes: "Stereo audio NR — same chain as AudioNrMono, applied independently to L/R. Used downstream of StereoDecoder in `wbfm_stereo`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        // Both channels should run at the same rate; prefer the left
        // port's declared rate and fall back to the right's if left is
        // unavailable.
        let rate = ctx.input_rate("left").or_else(|| ctx.input_rate("right"));
        if let Some(rate) = rate {
            if rate > 0.0 {
                self.left.refresh_rate(&self.params, rate);
                self.right.refresh_rate(&self.params, rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        let rate = ctx.input_rate("left").or_else(|| ctx.input_rate("right"));
        if let Some(rate) = rate {
            if rate > 0.0 {
                self.left.refresh_rate(&self.params, rate);
                self.right.refresh_rate(&self.params, rate);
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Resolve inputs up front so we can drop the immutable borrow
        // of `io.inputs` before writing to `io.outputs`.
        let left_in = io
            .inputs
            .iter()
            .find(|p| p.name == "left")
            .and_then(InputPort::as_real_f32)
            .map(<[f32]>::to_vec);
        let right_in = io
            .inputs
            .iter()
            .find(|p| p.name == "right")
            .and_then(InputPort::as_real_f32)
            .map(<[f32]>::to_vec);
        let (Some(left_src), Some(right_src)) = (left_in, right_in) else {
            return Ok(Work::new());
        };

        let mut w = Work::new();
        // Lock the per-channel frame count to the minimum across all
        // four ports so consumed/produced stays balanced — essential
        // for the scheduler's back-pressure accounting.
        let mut n = left_src.len().min(right_src.len());
        if let Some(left_out) = io
            .outputs
            .iter()
            .find(|p| p.name == "left")
            .and_then(|p| match &p.buf {
                crate::block::OutBuf::RealF32(buf) => Some(buf.len()),
                _ => None,
            })
        {
            n = n.min(left_out);
        }
        if let Some(right_out) =
            io.outputs
                .iter()
                .find(|p| p.name == "right")
                .and_then(|p| match &p.buf {
                    crate::block::OutBuf::RealF32(buf) => Some(buf.len()),
                    _ => None,
                })
        {
            n = n.min(right_out);
        }

        if let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "left")
            .and_then(OutputPort::as_real_f32_mut)
        {
            dst[..n].copy_from_slice(&left_src[..n]);
            self.left.run(&self.params, &mut dst[..n]);
        }
        if let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "right")
            .and_then(OutputPort::as_real_f32_mut)
        {
            dst[..n].copy_from_slice(&right_src[..n]);
            self.right.run(&self.params, &mut dst[..n]);
        }

        w.consumed[0] = n;
        w.consumed[1] = n;
        w.produced[0] = n;
        w.produced[1] = n;
        Ok(w)
    }
}

impl BlockFactory for AudioNrStereo {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AudioNrParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AudioNrStereo::new(p)?))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{AudioNrMono, AudioNrParams, AudioNrStereo};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};

    fn run_mono(block: &mut AudioNrMono, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0_f32; input.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = block.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    fn run_stereo(block: &mut AudioNrStereo, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let n = l.len().min(r.len());
        let mut out_l = vec![0.0_f32; n];
        let mut out_r = vec![0.0_f32; n];
        let mut inputs = [
            InputPort {
                name: "left",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&l[..n]),
            },
            InputPort {
                name: "right",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&r[..n]),
            },
        ];
        let mut outputs = [
            OutputPort {
                name: "left",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out_l),
            },
            OutputPort {
                name: "right",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out_r),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = block.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], n);
        assert_eq!(w.consumed[1], n);
        assert_eq!(w.produced[0], n);
        assert_eq!(w.produced[1], n);
        (out_l, out_r)
    }

    #[test]
    fn all_stages_disabled_is_bitwise_passthrough_mono() {
        let mut b = AudioNrMono::new(AudioNrParams::default()).unwrap();
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.1).sin()).collect();
        let out = run_mono(&mut b, &input);
        for (i, (a, c)) in input.iter().zip(out.iter()).enumerate() {
            assert_eq!(a.to_bits(), c.to_bits(), "sample {i}: {a} vs {c}");
        }
    }

    #[test]
    fn all_stages_disabled_is_bitwise_passthrough_stereo() {
        let mut b = AudioNrStereo::new(AudioNrParams::default()).unwrap();
        let l: Vec<f32> = (0..512).map(|i| (i as f32 * 0.1).sin()).collect();
        let r: Vec<f32> = (0..512).map(|i| (i as f32 * 0.13).cos()).collect();
        let (out_l, out_r) = run_stereo(&mut b, &l, &r);
        for i in 0..l.len() {
            assert_eq!(l[i].to_bits(), out_l[i].to_bits());
            assert_eq!(r[i].to_bits(), out_r[i].to_bits());
        }
    }

    #[test]
    fn deemph_enable_attenuates_treble() {
        use core::f32::consts::TAU;
        let fs = 48_000.0_f32;
        let f = 20_000.0_f32;
        let input: Vec<f32> = (0..4096).map(|i| (TAU * f * i as f32 / fs).cos()).collect();
        let mut b = AudioNrMono::new(AudioNrParams {
            deemph_enable: true,
            deemph_tau_us: 75.0,
            sample_rate_hz: fs,
            ..Default::default()
        })
        .unwrap();
        let out = run_mono(&mut b, &input);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        assert!(rms(&out[512..]) < rms(&input[512..]) * 0.5);
    }

    #[test]
    fn blanker_enable_kills_spike() {
        let mut input = vec![0.01_f32; 4096];
        input[2000] = 10.0;
        let mut b = AudioNrMono::new(AudioNrParams {
            blanker_enable: true,
            blanker_threshold_db: 20.0,
            blanker_hold_ms: 0.5,
            sample_rate_hz: 48_000.0,
            ..Default::default()
        })
        .unwrap();
        let out = run_mono(&mut b, &input);
        assert!(
            out[2000].abs() < 1e-6,
            "spike at sample 2000 should be blanked, got {}",
            out[2000]
        );
    }

    #[test]
    fn spectral_enable_attenuates_white_noise_mono() {
        let mut rng = 0xBADCAFE_u32;
        let input: Vec<f32> = (0..32_768)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();
        let mut b = AudioNrMono::new(AudioNrParams {
            spectral_enable: true,
            spectral_oversub: 2.0,
            spectral_floor: 0.1,
            spectral_noise_alpha: 0.1,
            sample_rate_hz: 48_000.0,
            ..Default::default()
        })
        .unwrap();
        let out = run_mono(&mut b, &input);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let warm = 2048;
        let suppression = 20.0 * (rms(&input[warm..]) / rms(&out[warm..])).log10();
        assert!(suppression > 3.0, "expected ≥3 dB, got {suppression:.1}");
    }

    #[test]
    fn neural_enable_attenuates_white_noise_at_48k() {
        // Mirror of the neural-stage test but exercised through the
        // block wiring — proves `neural_enable` + rate propagation +
        // params validation all line up end-to-end.
        let fs = 48_000.0_f32;
        let n = 2 * fs as usize;
        let mut rng = 0x42_42_42_42_u32;
        let input: Vec<f32> = (0..n)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();
        let mut b = AudioNrMono::new(AudioNrParams {
            neural_enable: true,
            neural_attenuation_db: 30.0,
            sample_rate_hz: fs,
            ..Default::default()
        })
        .unwrap();
        let out = run_mono(&mut b, &input);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let warm = 5 * super::neural::NEURAL_FRAME_SIZE;
        let db = 20.0 * (rms(&input[warm..]) / rms(&out[warm..])).log10();
        // RNNoise's response is input-dependent; a synthetic xorshift
        // stream isn't the richest noise source. 1 dB is the floor the
        // test defends against a *non-functional* neural path — real
        // speech-in-noise gets 10–15 dB.
        assert!(db > 1.0, "expected ≥1 dB RNNoise suppression, got {db:.1}");
    }

    #[test]
    fn rejects_invalid_params() {
        assert!(AudioNrMono::new(AudioNrParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AudioNrMono::new(AudioNrParams {
            spectral_enable: true,
            spectral_block_size: 500,
            ..Default::default()
        })
        .is_err());
        assert!(AudioNrMono::new(AudioNrParams {
            notch_enable: true,
            notch_taps: 1,
            ..Default::default()
        })
        .is_err());
    }
}
