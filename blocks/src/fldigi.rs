//! `FldigiDemod` — wraps the curated fldigi modem cores
//! ([`ferrite_fldigi`]) for live digital-mode decode.
//!
//! One generic block, mode selected by the `mode` param (`"rtty45"`,
//! later `"psk31"`, `"mt63-1000L"`, …). Unlike `WsprDemod`, the front-
//! end DSP lives *inside* the vendored modem — this block just streams
//! audio in and drains decoded output:
//!
//! * **text** → `tracing::info!(target: "decoder::fldigi", mode=…)`,
//!   the same log-stream transport WSPR/FT8/POCSAG use (no port).
//! * **scope** → drained and dropped *for now*; the ABI already
//!   carries it (RTTY crossed-ellipses / PSK phase vector) and a
//!   frontend scope widget is a later additive task — no decode/shim
//!   change needed then.
//!
//! ### Audio contract
//!
//! Mono real audio at the mode's native rate (RTTY: 8 kHz). The rate
//! is passed to the modem at construct time; `init()` warns once if
//! the scheduler-negotiated input rate disagrees (the decoder would
//! produce garbage).
//!
//! ### Placement
//!
//! `Placement::Either`. Native links the curated C++ statically; the
//! wasm side resolves the same C ABI against a sibling Emscripten
//! module (link-vs-bridge — see `blocks/native/fldigi`). Shipped
//! presets place it `node` so the decode happens once server-side and
//! fans out via the log stream.

use anyhow::{bail, Result};
use ferrite_fldigi::FldigiModem;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FldigiDemodParams {
    /// fldigi mode id. Phase 0 ships `"rtty45"`; the mode registry in
    /// `fldigi_shim.cxx` gates which strings construct.
    pub mode: String,
    /// Audio sample rate (Hz) handed to the modem. RTTY decodes at
    /// 8 kHz — feed a matching resampler upstream.
    pub sample_rate_hz: f32,
    /// Automatic frequency control (track drift). Maps to fldigi's
    /// `progStatus.afconoff` via the modem's `set_param`.
    pub afc: bool,
    /// RTTY mark/space polarity swap. RTTY is 2-tone FSK whose received
    /// polarity is genuinely ambiguous — it inverts with TX/RX sideband
    /// (HF RTTY is historically LSB; many SDRs/recordings are USB) or
    /// any odd number of spectral inversions in the chain. Every RTTY
    /// decoder has this Normal/Reverse control for exactly that reason;
    /// maps to fldigi's `progdefaults.rtty_reverse`. No effect on
    /// non-RTTY modes.
    pub rtty_reverse: bool,
    /// RX carrier the modem centres its filters on, Hz. `0.0` = leave
    /// fldigi's default (~1000 Hz / sweet-spot). Headless has no
    /// waterfall click and the shim's `powerDensity` stub returns 0,
    /// so the FSK/RTTY AFC sig-search can't lock — point the modem at
    /// the signal here (tune dial ≈ where the tones sit in the audio).
    pub rx_freq_hz: f32,
}

impl Default for FldigiDemodParams {
    fn default() -> Self {
        Self {
            mode: "rtty45".to_string(),
            sample_rate_hz: 8_000.0,
            afc: true,
            rtty_reverse: false,
            rx_freq_hz: 0.0,
        }
    }
}

pub struct FldigiDemod {
    params: FldigiDemodParams,
    modem: FldigiModem,
    warned_off_rate: bool,
}

impl FldigiDemod {
    pub fn new(params: FldigiDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz > 0.0) {
            bail!("FldigiDemod: sample_rate_hz must be positive");
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = params.sample_rate_hz as u32;
        let mut modem = FldigiModem::new(&params.mode, rate).ok_or_else(|| {
            anyhow::anyhow!("FldigiDemod: unknown/unsupported mode {:?}", params.mode)
        })?;
        modem.set_param("afc", if params.afc { 1.0 } else { 0.0 });
        modem.set_param("rtty_reverse", if params.rtty_reverse { 1.0 } else { 0.0 });
        // Always set (0 → fldigi default): the shim's waterfall stub is
        // a process global, so the carrier must be defined per-modem,
        // never inherited from a previously-constructed one.
        modem.set_param("rx_freq_hz", f64::from(params.rx_freq_hz));
        Ok(Self {
            params,
            modem,
            warned_off_rate: false,
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FldigiDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FldigiDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            // No output port — decodes reach the UI via the
            // `decoder::fldigi` tracing target (same as WSPR/FT8).
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "mode",
                    label: "Mode",
                    kind: ParamKind::Text {
                        default: "rtty45",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "fldigi mode id. Phase 0: \"rtty45\" (Baudot RTTY, 45.45 baud, 170 Hz shift). More modes (psk31, mt63-*, olivia-*, navtex, …) land against the same shim.",
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Audio rate",
                    kind: ParamKind::Range {
                        min: 8_000.0,
                        max: 8_000.0,
                        step: 1.0,
                        default: 8_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "RTTY decodes at 8 kHz. Feed a RealF32Resamp upstream so the channelizer/demod output matches.",
                },
                ParamSpec {
                    key: "afc",
                    label: "AFC",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Automatic frequency control — tracks carrier drift. Leave on unless the signal is rock-stable and you want a fixed bin.",
                },
                ParamSpec {
                    key: "rtty_reverse",
                    label: "RTTY reverse",
                    kind: ParamKind::Toggle { default: false },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "RTTY only: swap mark/space. RTTY polarity is ambiguous on receive (inverts with TX/RX sideband — HF RTTY is historically LSB, many SDRs are USB). If RTTY decodes as garbage but the two tones are clearly there, flip this. No effect on non-RTTY modes.",
                },
                ParamSpec {
                    key: "rx_freq_hz",
                    label: "RX carrier",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 4_000.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Audio carrier the modem centres on. 0 = fldigi default (~1000 Hz). Set to where the signal sits in the audio passband when AFC can't find it (FSK/RTTY headless: the powerDensity AFC search is inert, so point it manually).",
                },
            ],
            ai_notes: "fldigi digital-mode decoder (curated fldigi cores). Continuous decode (no slots). RTTY: tune so the two tones straddle the audio carrier; AFC pulls it in. Output: `tail decoder --category fldigi`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - f64::from(self.params.sample_rate_hz)).abs() > 1.0 {
                if !self.warned_off_rate {
                    tracing::warn!(
                        target: "decoder::fldigi",
                        mode = %self.params.mode,
                        rate_hz = rate,
                        expected = self.params.sample_rate_hz,
                        "fldigi_demod: input rate doesn't match the mode's audio rate — decoder will produce garbage. Add a RealF32Resamp upstream."
                    );
                    self.warned_off_rate = true;
                }
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut work = Work::new();
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(work);
        };
        if src.is_empty() {
            return Ok(work);
        }
        // Always consume — the modem buffers internally; never
        // back-pressure upstream.
        work.consumed[0] = src.len();

        self.modem.rx(src);

        let text = self.modem.take_text();
        if !text.is_empty() {
            tracing::info!(
                target: "decoder::fldigi",
                mode = %self.params.mode,
                "{}",
                text,
            );
        }
        // Scope frames are carried by the ABI for a future tuning-aid
        // widget; drain so they don't accumulate unbounded. Status
        // lines are fldigi UI chrome — not useful headless.
        let _ = self.modem.take_scope();
        let _ = self.modem.take_status();
        Ok(work)
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(afc) = delta.get("afc").and_then(serde_json::Value::as_bool) {
            self.params.afc = afc;
            self.modem.set_param("afc", if afc { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(rev) = delta
            .get("rtty_reverse")
            .and_then(serde_json::Value::as_bool)
        {
            self.params.rtty_reverse = rev;
            self.modem
                .set_param("rtty_reverse", if rev { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
            #[allow(clippy::cast_possible_truncation)]
            {
                self.params.rx_freq_hz = f as f32;
            }
            if f > 0.0 {
                self.modem.set_param("rx_freq_hz", f);
            }
            changed = true;
        }
        Ok(changed)
    }
}

impl BlockFactory for FldigiDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FldigiDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FldigiDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{FldigiDemod, FldigiDemodParams};
    use crate::block::Block;

    #[test]
    fn spec_is_either_real_in_no_outputs() {
        let s = FldigiDemod::spec();
        assert_eq!(s.type_name, "FldigiDemod");
        assert!(matches!(s.placement, crate::block::Placement::Either));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn default_params_construct_rtty() {
        let _ = FldigiDemod::new(FldigiDemodParams::default()).unwrap();
    }

    #[test]
    fn unknown_mode_errors() {
        let p = FldigiDemodParams {
            mode: "not-a-mode".to_string(),
            ..FldigiDemodParams::default()
        };
        assert!(FldigiDemod::new(p).is_err());
    }

    #[test]
    fn rejects_bad_rate() {
        let p = FldigiDemodParams {
            sample_rate_hz: 0.0,
            ..FldigiDemodParams::default()
        };
        assert!(FldigiDemod::new(p).is_err());
    }
}
