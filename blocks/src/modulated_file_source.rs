//! `ModulatedFileSource` — replay a sample as a realistic RF signal.
//!
//! A single source block (one `src` slot, no preset/compose change)
//! that wraps the loopback chain the e2e suite proved out:
//!
//! * **audio fixture** → [`FileAudioSource`] → AM/FM/SSB modulator →
//!   IQ. Demodulated-audio captures (`samples/audio/*`, the fldigi/
//!   multimon mono WAVs) become an on-air signal so the *whole* RX
//!   pipeline (channelizer → demod → decoder) runs, not a raw-audio
//!   bypass.
//! * **IQ capture** → [`FileIqSource`] → [`IqUpmix`] (shift by
//!   `offset_hz`; 0 = straight replay, non-zero exercises the
//!   channelizer's tuner).
//!
//! Internally it owns the two inner blocks and pumps them — the same
//! "one block hides a sub-chain" pattern as `FldigiAuto`. Output is
//! `iq_f32` at `output_rate_hz`; the picker (`GET /api/captures`)
//! supplies `kind`/rate from each clip's sidecar.

#![allow(clippy::too_many_lines, clippy::assigning_clones)]

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::am_modulator::{AmModulator, AmModulatorParams};
use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort,
    ParamKind, ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::file_audio_source::{FileAudioSource, FileAudioSourceParams};
use crate::file_source::{FileIqSource, FileIqSourceParams};
use crate::fm_modulator::{FmModulator, FmModulatorParams};
use crate::iq_upmix::{IqUpmix, IqUpmixParams};
use crate::ssb_modulator::{Sideband, SsbModulator, SsbModulatorParams};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModulatedFileSourceParams {
    pub path: std::path::PathBuf,
    /// `"audio"` (→ modulate) or `"iq"` (→ upmix). The captures picker
    /// fills this from the sidecar `format`.
    pub kind: String,
    /// Audio kind only: `"am"` | `"fm"` | `"ssb"`.
    pub modulation: String,
    /// SSB sideband (`"usb"`/`"lsb"`) when `modulation = "ssb"`.
    pub sideband: String,
    /// Carrier offset (audio: modulator carrier; iq: `IqUpmix` shift).
    pub offset_hz: f32,
    /// FM peak deviation (Hz) when `modulation = "fm"`.
    pub deviation_hz: f32,
    /// AM modulation depth when `modulation = "am"`.
    pub mod_depth: f32,
    /// Emitted IQ sample rate.
    pub output_rate_hz: f32,
    /// Rate for headerless raw inputs (`cf32`/`.iq`); WAV uses its
    /// header. From the sidecar `sample_rate_hz`.
    pub rate_hz_hint: f64,
    /// Reported downstream as the IQ centre (metadata only).
    pub center_freq_hz: f64,
    pub loop_playback: bool,
}

impl Default for ModulatedFileSourceParams {
    fn default() -> Self {
        Self {
            path: std::path::PathBuf::new(),
            kind: "audio".to_string(),
            modulation: "fm".to_string(),
            sideband: "usb".to_string(),
            offset_hz: 12_000.0,
            deviation_hz: 5_000.0,
            mod_depth: 0.7,
            output_rate_hz: 240_000.0,
            rate_hz_hint: 0.0,
            center_freq_hz: 0.0,
            loop_playback: true,
        }
    }
}

/// Source-side scratch chunk pulled from the inner file source per
/// refill (well above any modulator's per-call appetite).
const SRC_CHUNK: usize = 16_384;

enum Pipe {
    /// `FileAudioSource → {Am,Fm,Ssb}Modulator`.
    Audio {
        src: FileAudioSource,
        modu: Box<dyn Block>,
        buf: Vec<f32>,
        pos: usize,
        eof: bool,
    },
    /// `FileIqSource → IqUpmix`.
    Iq {
        src: FileIqSource,
        up: IqUpmix,
        buf: Vec<Complex<f32>>,
        pos: usize,
        eof: bool,
    },
}

pub struct ModulatedFileSource {
    params: ModulatedFileSourceParams,
    pipe: Pipe,
}

impl ModulatedFileSource {
    pub fn new(params: ModulatedFileSourceParams) -> Result<Self> {
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!("modulated_file_source output_rate_hz must be > 0");
        }
        let pipe = match params.kind.as_str() {
            "audio" => {
                let src = FileAudioSource::new(&FileAudioSourceParams {
                    path: params.path.clone(),
                    rate_hz_hint: params.rate_hz_hint,
                    gain: 1.0,
                    loop_playback: params.loop_playback,
                })?;
                #[allow(clippy::cast_possible_truncation)]
                let in_rate = src.rate_hz() as f32;
                let modu: Box<dyn Block> = match params.modulation.as_str() {
                    "am" => Box::new(AmModulator::new(AmModulatorParams {
                        input_rate_hz: in_rate,
                        output_rate_hz: params.output_rate_hz,
                        offset_hz: params.offset_hz,
                        mod_depth: params.mod_depth,
                    })?),
                    "fm" => Box::new(FmModulator::new(FmModulatorParams {
                        input_rate_hz: in_rate,
                        output_rate_hz: params.output_rate_hz,
                        offset_hz: params.offset_hz,
                        deviation_hz: params.deviation_hz,
                    })?),
                    "ssb" => {
                        let sb = match params.sideband.as_str() {
                            "lsb" => Sideband::Lsb,
                            _ => Sideband::Usb,
                        };
                        Box::new(SsbModulator::new(SsbModulatorParams {
                            input_rate_hz: in_rate,
                            output_rate_hz: params.output_rate_hz,
                            offset_hz: params.offset_hz,
                            sideband: sb,
                        })?)
                    }
                    other => bail!(
                        "modulated_file_source: kind=audio needs modulation am|fm|ssb, got {other:?}"
                    ),
                };
                Pipe::Audio {
                    src,
                    modu,
                    buf: Vec::new(),
                    pos: 0,
                    eof: false,
                }
            }
            "iq" => {
                let src = FileIqSource::new(&FileIqSourceParams {
                    path: params.path.clone(),
                    rate_hz_hint: params.rate_hz_hint,
                    center_freq_hz: params.center_freq_hz,
                    loop_playback: params.loop_playback,
                })?;
                #[allow(clippy::cast_possible_truncation)]
                let in_rate = src.rate_hz() as f32;
                let up = IqUpmix::new(IqUpmixParams {
                    shift_hz: params.offset_hz,
                    sample_rate_hz: in_rate,
                })?;
                Pipe::Iq {
                    src,
                    up,
                    buf: Vec::new(),
                    pos: 0,
                    eof: false,
                }
            }
            other => bail!("modulated_file_source: kind must be audio|iq, got {other:?}"),
        };
        Ok(Self { params, pipe })
    }
}

/// Drive one inner-`Block` step: `in_buf` → `out`, returning
/// `(consumed, produced)`. `IN`/`OUT` are the port `InBuf`/`OutBuf`
/// constructors so this works for both real→iq and iq→iq stages.
macro_rules! step {
    ($stage:expr, $in_buf:expr, $in_ctor:path, $out:expr, $out_ctor:path) => {{
        let mut inp = [InputPort {
            name: "in",
            meta: crate::block::PortMeta::default(),
            buf: $in_ctor($in_buf),
        }];
        let mut outp = [OutputPort {
            name: "out",
            meta: crate::block::PortMeta::default(),
            buf: $out_ctor($out),
        }];
        let w = $stage
            .process(&mut BlockIo {
                inputs: &mut inp,
                outputs: &mut outp,
            })
            .unwrap_or_else(|_| Work::new());
        (w.consumed[0], w.produced[0])
    }};
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for ModulatedFileSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "ModulatedFileSource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "path",
                    label: "Path",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Server path to a capture (from GET /api/captures).",
                },
                ParamSpec {
                    key: "kind",
                    label: "Kind",
                    kind: ParamKind::EnumString {
                        values: &["audio", "iq"],
                        default: "audio",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "audio → modulate to IQ; iq → upmix. From the sidecar format.",
                },
                ParamSpec {
                    key: "modulation",
                    label: "Modulation",
                    kind: ParamKind::EnumString {
                        values: &["am", "fm", "ssb"],
                        default: "fm",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "audio kind only: how to put the clip on a carrier. Match the real signal class (AFSK1200/POCSAG = fm, fldigi/FT8 = ssb, broadcast AM = am).",
                },
                ParamSpec {
                    key: "sideband",
                    label: "Sideband",
                    kind: ParamKind::EnumString {
                        values: &["usb", "lsb"],
                        default: "usb",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "SSB only.",
                },
                ParamSpec {
                    key: "offset_hz",
                    label: "Carrier / shift",
                    kind: ParamKind::Range {
                        min: -1_000_000.0,
                        max: 1_000_000.0,
                        step: 1.0,
                        default: 12_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio: modulator carrier offset. IQ: IqUpmix shift (0 = straight replay; non-zero makes the channelizer tune).",
                },
                ParamSpec {
                    key: "deviation_hz",
                    label: "FM deviation",
                    kind: ParamKind::Range {
                        min: 100.0,
                        max: 200_000.0,
                        step: 100.0,
                        default: 5_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "FM only. NBFM voice/data ≈ 5 kHz; WBFM 75 kHz.",
                },
                ParamSpec {
                    key: "mod_depth",
                    label: "AM depth",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.01,
                        default: 0.7,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "AM only.",
                },
                ParamSpec {
                    key: "output_rate_hz",
                    label: "IQ rate",
                    kind: ParamKind::Range {
                        min: 8_000.0,
                        max: 20_000_000.0,
                        step: 1.0,
                        default: 240_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Emitted IQ rate — match the preset's source rate.",
                },
                ParamSpec {
                    key: "loop",
                    label: "Loop",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Rewind at EOF for continuous replay.",
                },
            ],
            ai_notes: "Replay a sample as a real RF signal: audio fixtures get modulated (am/fm/ssb), IQ captures upmixed — so the full RX chain runs, not a raw bypass. Pairs with GET /api/captures. Loopback/test source; not in any listen preset.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        (1, 0) // pure source — decoupled from any input
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(f64::from(self.params.output_rate_hz))
    }

    fn output_center_freq_hz(&self, _port: usize) -> Option<f64> {
        Some(self.params.center_freq_hz)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let n = io
            .outputs
            .iter()
            .find(|p| p.name == "out")
            .and_then(|p| match &p.buf {
                OutBuf::IqF32(s) => Some(s.len()),
                _ => None,
            })
            .unwrap_or(0);
        if n == 0 {
            return Ok(Work::new());
        }
        let out = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
            .expect("out len checked");

        let mut produced = 0usize;
        match &mut self.pipe {
            Pipe::Audio {
                src,
                modu,
                buf,
                pos,
                eof,
            } => {
                while produced < n && !(*eof && *pos >= buf.len()) {
                    if *pos >= buf.len() && !*eof {
                        buf.resize(SRC_CHUNK, 0.0);
                        let mut sp = [OutputPort {
                            name: "out",
                            meta: crate::block::PortMeta::default(),
                            buf: OutBuf::RealF32(buf),
                        }];
                        let w = src.process(&mut BlockIo {
                            inputs: &mut [],
                            outputs: &mut sp,
                        })?;
                        buf.truncate(w.produced[0]);
                        *pos = 0;
                        if src.is_eof() {
                            *eof = true;
                        }
                        if buf.is_empty() {
                            break;
                        }
                        continue;
                    }
                    let (c, p) = step!(
                        modu,
                        &buf[*pos..],
                        InBuf::RealF32,
                        &mut out[produced..n],
                        OutBuf::IqF32
                    );
                    *pos += c;
                    produced += p;
                    if c == 0 && p == 0 {
                        break;
                    }
                }
            }
            Pipe::Iq {
                src,
                up,
                buf,
                pos,
                eof,
            } => {
                while produced < n && !(*eof && *pos >= buf.len()) {
                    if *pos >= buf.len() && !*eof {
                        buf.resize(SRC_CHUNK, Complex::new(0.0, 0.0));
                        let mut sp = [OutputPort {
                            name: "out",
                            meta: crate::block::PortMeta::default(),
                            buf: OutBuf::IqF32(buf),
                        }];
                        let w = src.process(&mut BlockIo {
                            inputs: &mut [],
                            outputs: &mut sp,
                        })?;
                        buf.truncate(w.produced[0]);
                        *pos = 0;
                        if src.is_eof() {
                            *eof = true;
                        }
                        if buf.is_empty() {
                            break;
                        }
                        continue;
                    }
                    let (c, p) = step!(
                        up,
                        &buf[*pos..],
                        InBuf::IqF32,
                        &mut out[produced..n],
                        OutBuf::IqF32
                    );
                    *pos += c;
                    produced += p;
                    if c == 0 && p == 0 {
                        break;
                    }
                }
            }
        }

        let mut w = Work::new();
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for ModulatedFileSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(ModulatedFileSource::new(
            crate::block::deserialize_params(params)?,
        )?))
    }
}
