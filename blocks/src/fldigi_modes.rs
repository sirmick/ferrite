//! Per-mode fldigi decoder blocks over a shared [`FldigiCore`].
//!
//! Replaces the single generic `FldigiDemod(mode="…")` with one block
//! per fldigi mode *family* (`RttyDemod`, `Psk31Demod`, `Mt63Demod`,
//! …). Each is a thin wrapper: it owns a `FldigiCore` (construct + rx +
//! drain + tracing + rate-check, factored once) and exposes *typed,
//! labelled* params for that mode. Config params forward through the
//! shim's generic CONFIG_LIST passthrough (fldigi TAG strings); the
//! three non-config runtime seams keep their special keys
//! (`afc`, `rtty_reverse`, `rx_freq_hz`).
//!
//! `make_modem` in the shim is already per-family (variants are
//! constructor args), so this maps 1:1. Decoded text reaches the UI
//! via the same `decoder::fldigi` tracing target as before.
//!
//! Mode coverage is grown incrementally, each new block landing with a
//! real-sample e2e gate (see `blocks/tests/fldigi_modes_e2e.rs`).

// Block constructors take their `Params` by value (codebase-wide
// convention even when not every field moves into `self`); doc
// comments reference fldigi TAGs / mode ids.
#![allow(clippy::needless_pass_by_value, clippy::doc_markdown)]

use anyhow::{bail, Result};
use ferrite_fldigi::FldigiModem;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// fldigi modes run at 8 kHz in this shim; presets feed a
/// `RealF32Resamp` upstream to match.
const FLDIGI_RATE_HZ: f32 = 8_000.0;

/// Shared decode engine: owns the vendored modem and does the
/// construct / rx / drain / tracing / rate-warn that every per-mode
/// block needs identically.
pub struct FldigiCore {
    modem: FldigiModem,
    /// Shown in the `mode=` tracing field (the family or variant id).
    label: String,
    warned_off_rate: bool,
}

impl FldigiCore {
    /// Build the modem for `mode_id` (the shim's `make_modem` id, e.g.
    /// `"rtty45"`, `"mt63-1000L"`). `label` is what appears in the
    /// `decoder::fldigi mode=` field.
    pub fn new(mode_id: &str, label: impl Into<String>) -> Result<Self> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = FLDIGI_RATE_HZ as u32; // 8000.0 — exact, positive
        let modem = FldigiModem::new(mode_id, rate)
            .ok_or_else(|| anyhow::anyhow!("fldigi: unknown/unsupported mode {mode_id:?}"))?;
        Ok(Self {
            modem,
            label: label.into(),
            warned_off_rate: false,
        })
    }

    /// Forward a knob to the modem. `key` is a fldigi config TAG
    /// (generic passthrough) or one of the three runtime seams
    /// (`afc`, `rtty_reverse`, `rx_freq_hz`).
    pub fn set(&mut self, key: &str, v: f64) {
        self.modem.set_param(key, v);
    }

    /// Rebuild the inner modem as a different mode (same 8 kHz rate),
    /// keeping the block instance/ports/wiring. The old `FldigiModem`
    /// is dropped (its `Drop` destroys the C handle, incl. any RSID
    /// detector). Used by `FldigiAuto` on an RSID hit — the mode swap
    /// is entirely internal, never a graph change.
    pub fn switch_mode(&mut self, mode_id: &str, label: impl Into<String>) -> Result<()> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = FLDIGI_RATE_HZ as u32;
        let modem = FldigiModem::new(mode_id, rate)
            .ok_or_else(|| anyhow::anyhow!("fldigi: unknown/unsupported mode {mode_id:?}"))?;
        self.modem = modem;
        self.label = label.into();
        Ok(())
    }

    /// Drain RSID mode-id detections since the last call (empty unless
    /// RSID was enabled on this modem via `set("RECEIVERSID", 1.0)`).
    pub fn take_rsid(&mut self) -> Vec<String> {
        self.modem.take_rsid()
    }

    /// The current mode label (`decoder::fldigi mode=` field).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Standard fldigi knobs every block exposes: AFC + manual RX
    /// carrier (headless AFC sig-search is inert — see the shim).
    pub fn apply_common(&mut self, afc: bool, rx_freq_hz: f32) {
        self.modem.set_param("afc", if afc { 1.0 } else { 0.0 });
        // Always set (0 → fldigi default): the shim's waterfall stub is
        // a process global; carrier must be defined per-modem.
        self.modem.set_param("rx_freq_hz", f64::from(rx_freq_hz));
    }

    /// Mark/space (sideband) inversion. The `rtty_reverse` key drives
    /// the waterfall stub's `Reverse()`, which `modem::rx_init`
    /// re-derives `reverse` from for *every* modem (not just RTTY) —
    /// so MFSK/FSK families invert with sideband the same way.
    pub fn apply_reverse(&mut self, reverse: bool) {
        self.modem
            .set_param("rtty_reverse", if reverse { 1.0 } else { 0.0 });
    }

    fn warn_if_off_rate(&mut self, ctx: &InitCtx<'_>) {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - f64::from(FLDIGI_RATE_HZ)).abs() > 1.0 && !self.warned_off_rate
            {
                tracing::warn!(
                    target: "decoder::fldigi",
                    mode = %self.label,
                    rate_hz = rate,
                    expected = FLDIGI_RATE_HZ,
                    "fldigi: input rate doesn't match the 8 kHz modem rate — decoder will produce garbage. Add a RealF32Resamp upstream."
                );
                self.warned_off_rate = true;
            }
        }
    }

    /// Consume the input, run the modem, emit decoded text to the
    /// `decoder::fldigi` tracing target. Always consumes (the modem
    /// buffers internally — never back-pressures upstream).
    fn pump(&mut self, io: &mut BlockIo<'_>) -> Work {
        let mut work = Work::new();
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return work;
        };
        if src.is_empty() {
            return work;
        }
        work.consumed[0] = src.len();
        self.modem.rx(src);

        let text = self.modem.take_text();
        if !text.is_empty() {
            tracing::info!(target: "decoder::fldigi", mode = %self.label, "{}", text);
        }
        // Scope: future tuning-aid widget; status: fldigi UI chrome.
        // Drain both so they don't grow unbounded.
        let _ = self.modem.take_scope();
        let _ = self.modem.take_status();
        work
    }
}

/// Real-audio input port shared by every fldigi block (8 kHz mono).
const FLDIGI_IN: &[PortSpec] = &[PortSpec {
    name: "in",
    port_type: PortType::RealF32,
}];

/// The two knobs every fldigi block carries (const so they can back a
/// `&'static [ParamSpec]` via rvalue static promotion).
const AFC_PARAM: ParamSpec = ParamSpec {
    key: "afc",
    label: "AFC",
    kind: ParamKind::Toggle { default: true },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Automatic frequency control — tracks carrier drift. Leave on unless the signal is rock-stable.",
};
const RX_FREQ_PARAM: ParamSpec = ParamSpec {
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
    ai_notes: "Audio carrier the modem centres on. 0 = fldigi default (~1000 Hz). Headless AFC sig-search is inert, so point it at where the signal sits when AFC can't find it.",
};
/// Sideband/polarity inversion — relevant to every FSK/MFSK family
/// (RTTY, Olivia, Contestia, DominoEX, Throb, NAVTEX), not just RTTY.
const REVERSE_PARAM: ParamSpec = ParamSpec {
    key: "reverse",
    label: "Reverse",
    kind: ParamKind::Toggle { default: false },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Swap mark/space. Tone order inverts with TX/RX sideband (HF data is historically LSB, many SDRs USB). Flip if the tones are clearly present but the text is garbage.",
};

// ---------------------------------------------------------------------
// RttyDemod — Baudot/ITA2 RTTY (45.45 baud, 170 Hz shift).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RttyDemodParams {
    pub afc: bool,
    pub rx_freq_hz: f32,
    /// Swap mark/space. RTTY RX polarity is genuinely ambiguous —
    /// inverts with TX/RX sideband (HF RTTY is historically LSB, many
    /// SDRs are USB). Flip if the two tones are clearly present but the
    /// text is garbage.
    pub reverse: bool,
}

impl Default for RttyDemodParams {
    fn default() -> Self {
        Self {
            afc: true,
            rx_freq_hz: 0.0,
            reverse: false,
        }
    }
}

pub struct RttyDemod {
    core: FldigiCore,
}

impl RttyDemod {
    pub fn new(p: RttyDemodParams) -> Result<Self> {
        let mut core = FldigiCore::new("rtty45", "rtty45")?;
        core.apply_common(p.afc, p.rx_freq_hz);
        core.apply_reverse(p.reverse);
        Ok(Self { core })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for RttyDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "RttyDemod",
            placement: Placement::Either,
            inputs: FLDIGI_IN,
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "afc",
                    label: "AFC",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Automatic frequency control — tracks carrier drift.",
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
                    ai_notes: "0 = fldigi default. Headless AFC is inert; set to where the FSK pair sits if it won't lock.",
                },
                ParamSpec {
                    key: "reverse",
                    label: "Reverse",
                    kind: ParamKind::Toggle { default: false },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Swap mark/space. RTTY polarity inverts with TX/RX sideband — flip if the tones are there but text is garbage.",
                },
            ],
            ai_notes: "RTTY (Baudot 45.45 bd / 170 Hz shift) via the curated fldigi rtty core. Tune so the two tones straddle the audio carrier; `reverse` if inverted. Output: `tail decoder --category fldigi`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.core.warn_if_off_rate(ctx);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.core.pump(io))
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(afc) = delta.get("afc").and_then(serde_json::Value::as_bool) {
            self.core.set("afc", if afc { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(rev) = delta.get("reverse").and_then(serde_json::Value::as_bool) {
            self.core.set("rtty_reverse", if rev { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
            self.core.set("rx_freq_hz", f);
            changed = true;
        }
        Ok(changed)
    }
}

impl BlockFactory for RttyDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(RttyDemod::new(crate::block::deserialize_params(
            params,
        )?)?))
    }
}

// ---------------------------------------------------------------------
// Psk31Demod — BPSK31. Wide capture / AFC: no extra knobs.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Psk31DemodParams {
    pub afc: bool,
    pub rx_freq_hz: f32,
}

impl Default for Psk31DemodParams {
    fn default() -> Self {
        Self {
            afc: true,
            rx_freq_hz: 0.0,
        }
    }
}

pub struct Psk31Demod {
    core: FldigiCore,
}

impl Psk31Demod {
    pub fn new(p: Psk31DemodParams) -> Result<Self> {
        let mut core = FldigiCore::new("psk31", "psk31")?;
        core.apply_common(p.afc, p.rx_freq_hz);
        Ok(Self { core })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Psk31Demod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Psk31Demod",
            placement: Placement::Either,
            inputs: FLDIGI_IN,
            outputs: &[],
            params: &[AFC_PARAM, RX_FREQ_PARAM],
            ai_notes: "BPSK31 via the curated fldigi psk core. Narrow, AFC pulls it in. Output: `tail decoder --category fldigi`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.core.warn_if_off_rate(ctx);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.core.pump(io))
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(afc) = delta.get("afc").and_then(serde_json::Value::as_bool) {
            self.core.set("afc", if afc { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
            self.core.set("rx_freq_hz", f);
            changed = true;
        }
        Ok(changed)
    }
}

impl BlockFactory for Psk31Demod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(Psk31Demod::new(
            crate::block::deserialize_params(params)?,
        )?))
    }
}

// ---------------------------------------------------------------------
// CwDemod — Morse via the curated fldigi cw core (the `_cw_live`
// preset's decoder; distinct from the multimon `MorseDemod`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CwDemodParams {
    pub afc: bool,
    pub rx_freq_hz: f32,
}

impl Default for CwDemodParams {
    fn default() -> Self {
        Self {
            afc: true,
            rx_freq_hz: 0.0,
        }
    }
}

pub struct CwDemod {
    core: FldigiCore,
}

impl CwDemod {
    pub fn new(p: CwDemodParams) -> Result<Self> {
        let mut core = FldigiCore::new("cw", "cw")?;
        core.apply_common(p.afc, p.rx_freq_hz);
        Ok(Self { core })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for CwDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "CwDemod",
            placement: Placement::Either,
            inputs: FLDIGI_IN,
            outputs: &[],
            params: &[AFC_PARAM, RX_FREQ_PARAM],
            ai_notes: "Morse/CW via the curated fldigi cw core (decode-side; the multimon MorseDemod is the other CW path). Output: `tail decoder --category fldigi`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.core.warn_if_off_rate(ctx);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.core.pump(io))
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(afc) = delta.get("afc").and_then(serde_json::Value::as_bool) {
            self.core.set("afc", if afc { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
            self.core.set("rx_freq_hz", f);
            changed = true;
        }
        Ok(changed)
    }
}

impl BlockFactory for CwDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(CwDemod::new(crate::block::deserialize_params(
            params,
        )?)?))
    }
}

// ---------------------------------------------------------------------
// Mt63Demod — MT63, variant-selected (64-tone DBPSK + interleave/FEC).
// ---------------------------------------------------------------------

const MT63_VARIANTS: &[&str] = &[
    "mt63-500S",
    "mt63-500L",
    "mt63-1000S",
    "mt63-1000L",
    "mt63-2000S",
    "mt63-2000L",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Mt63DemodParams {
    /// One of [`MT63_VARIANTS`]. Bandwidth-S/L = short/long interleave.
    pub variant: String,
    pub afc: bool,
    pub rx_freq_hz: f32,
}

impl Default for Mt63DemodParams {
    fn default() -> Self {
        Self {
            variant: "mt63-1000L".to_string(),
            afc: true,
            rx_freq_hz: 0.0,
        }
    }
}

pub struct Mt63Demod {
    core: FldigiCore,
}

impl Mt63Demod {
    pub fn new(p: Mt63DemodParams) -> Result<Self> {
        if !MT63_VARIANTS.contains(&p.variant.as_str()) {
            bail!(
                "Mt63Demod: variant must be one of {MT63_VARIANTS:?}, got {:?}",
                p.variant
            );
        }
        let mut core = FldigiCore::new(&p.variant, p.variant.clone())?;
        core.apply_common(p.afc, p.rx_freq_hz);
        Ok(Self { core })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Mt63Demod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Mt63Demod",
            placement: Placement::Either,
            inputs: FLDIGI_IN,
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "variant",
                    label: "Variant",
                    kind: ParamKind::EnumString {
                        values: MT63_VARIANTS,
                        default: "mt63-1000L",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Bandwidth + interleave: 500/1000/2000 Hz, S(hort)/L(ong) interleave. 1000L is the EmComm/MARS workhorse.",
                },
                AFC_PARAM,
                RX_FREQ_PARAM,
            ],
            ai_notes: "MT63 (64-tone DBPSK, interleave+FEC) via the curated fldigi mt63 core. Several-second decode latency by design. Tune the block ~1500 Hz if AFC can't find it. Output: `tail decoder --category fldigi`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.core.warn_if_off_rate(ctx);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.core.pump(io))
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(afc) = delta.get("afc").and_then(serde_json::Value::as_bool) {
            self.core.set("afc", if afc { 1.0 } else { 0.0 });
            changed = true;
        }
        if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
            self.core.set("rx_freq_hz", f);
            changed = true;
        }
        Ok(changed)
    }
}

impl BlockFactory for Mt63Demod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(Mt63Demod::new(crate::block::deserialize_params(
            params,
        )?)?))
    }
}

// ---------------------------------------------------------------------
// Variant-selected MFSK/FSK families. Structurally identical wrappers
// (variant + afc + rx_freq + reverse → FldigiCore), so generated from
// one macro rather than five near-duplicate copies. Variant ids are
// the shim `make_modem` strings verbatim.
// ---------------------------------------------------------------------

macro_rules! family_demod {
    ($params:ident, $block:ident, $type_name:literal,
     $variants:ident = [$($v:literal),+ $(,)?], $default:literal, $ai:literal) => {
        const $variants: &[&str] = &[$($v),+];

        #[derive(Debug, Clone, Deserialize)]
        #[serde(default)]
        pub struct $params {
            /// One of the family's `make_modem` ids.
            pub variant: String,
            pub afc: bool,
            pub rx_freq_hz: f32,
            pub reverse: bool,
        }
        impl Default for $params {
            fn default() -> Self {
                Self {
                    variant: $default.to_string(),
                    afc: true,
                    rx_freq_hz: 0.0,
                    reverse: false,
                }
            }
        }

        pub struct $block {
            core: FldigiCore,
        }
        impl $block {
            pub fn new(p: $params) -> Result<Self> {
                if !$variants.contains(&p.variant.as_str()) {
                    bail!(
                        concat!($type_name, ": variant must be one of {:?}, got {:?}"),
                        $variants,
                        p.variant
                    );
                }
                let mut core = FldigiCore::new(&p.variant, p.variant.clone())?;
                core.apply_common(p.afc, p.rx_freq_hz);
                core.apply_reverse(p.reverse);
                Ok(Self { core })
            }
        }

        #[ferrite_blocks_macros::ferrite_block]
        impl Block for $block {
            fn spec() -> BlockSpec {
                BlockSpec {
                    type_name: $type_name,
                    placement: Placement::Either,
                    inputs: FLDIGI_IN,
                    outputs: &[],
                    params: &[
                        ParamSpec {
                            key: "variant",
                            label: "Variant",
                            kind: ParamKind::EnumString {
                                values: $variants,
                                default: $default,
                            },
                            reconfig_scope: ReconfigureScope::SourceRestart,
                            ai_notes: "Mode variant (bandwidth / tones / interleave).",
                        },
                        AFC_PARAM,
                        RX_FREQ_PARAM,
                        REVERSE_PARAM,
                    ],
                    ai_notes: $ai,
                }
            }
            fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
                self.core.warn_if_off_rate(ctx);
                Ok(())
            }
            fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
                Ok(self.core.pump(io))
            }
            fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
                let mut changed = false;
                if let Some(b) = delta.get("afc").and_then(serde_json::Value::as_bool) {
                    self.core.set("afc", if b { 1.0 } else { 0.0 });
                    changed = true;
                }
                if let Some(b) = delta.get("reverse").and_then(serde_json::Value::as_bool) {
                    self.core.apply_reverse(b);
                    changed = true;
                }
                if let Some(f) = delta.get("rx_freq_hz").and_then(serde_json::Value::as_f64) {
                    self.core.set("rx_freq_hz", f);
                    changed = true;
                }
                Ok(changed)
            }
        }

        impl BlockFactory for $block {
            fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
                Ok(Box::new($block::new(crate::block::deserialize_params(params)?)?))
            }
        }
    };
}

family_demod!(
    OliviaDemodParams, OliviaDemod, "OliviaDemod",
    OLIVIA_VARIANTS = ["olivia", "olivia-8-500", "olivia-16-500", "olivia-32-1000"],
    "olivia-8-500",
    "Olivia (MFSK + Reed-Solomon FEC) via the curated fldigi olivia core. Very robust, slow. Tolerant of mistuning; AFC usually pulls it in. Output: `tail decoder --category fldigi`."
);
family_demod!(
    ContestiaDemodParams, ContestiaDemod, "ContestiaDemod",
    CONTESTIA_VARIANTS = ["contestia", "contestia-8-250", "contestia-8-500", "contestia-16-500"],
    "contestia-8-500",
    "Contestia (Olivia-derivative MFSK+FEC) via the curated fldigi contestia core. Tolerant of mistuning. Output: `tail decoder --category fldigi`."
);
family_demod!(
    DominoexDemodParams, DominoexDemod, "DominoexDemod",
    DOMINOEX_VARIANTS = [
        "dominoex4", "dominoex8", "dominoex11", "dominoex16", "dominoex22", "dominoex44"
    ],
    "dominoex16",
    "DominoEX (incremental-frequency MFSK) via the curated fldigi dominoex core. Sideband-sensitive — try `reverse` if garbled. Output: `tail decoder --category fldigi`."
);
family_demod!(
    ThrobDemodParams, ThrobDemod, "ThrobDemod",
    THROB_VARIANTS = ["throb1", "throb2", "throb4", "throbx1", "throbx2", "throbx4"],
    "throb4",
    "Throb / ThrobX (slow MFSK) via the curated fldigi throb core. Output: `tail decoder --category fldigi`."
);
family_demod!(
    NavtexDemodParams, NavtexDemod, "NavtexDemod",
    NAVTEX_VARIANTS = ["navtex", "sitorb"],
    "navtex",
    "NAVTEX / SITOR-B (CCIR-476, 100 bd / 170 Hz shift, time-diversity FEC) via the curated fldigi navtex core. 518 kHz maritime safety. Output: `tail decoder --category fldigi`."
);

#[cfg(test)]
mod tests {
    use super::{
        Mt63Demod, Mt63DemodParams, Psk31Demod, Psk31DemodParams, RttyDemod, RttyDemodParams,
    };
    use crate::block::Block;

    #[test]
    fn specs_are_real_in_no_outputs() {
        for (name, s) in [
            ("RttyDemod", RttyDemod::spec()),
            ("Psk31Demod", Psk31Demod::spec()),
            ("Mt63Demod", Mt63Demod::spec()),
        ] {
            assert_eq!(s.type_name, name);
            assert_eq!(s.inputs.len(), 1);
            assert_eq!(s.outputs.len(), 0);
            assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
        }
    }

    #[test]
    fn defaults_construct() {
        let _ = RttyDemod::new(RttyDemodParams::default()).unwrap();
        let _ = Psk31Demod::new(Psk31DemodParams::default()).unwrap();
        let _ = Mt63Demod::new(Mt63DemodParams::default()).unwrap();
    }

    #[test]
    fn mt63_rejects_unknown_variant() {
        let p = Mt63DemodParams {
            variant: "mt63-9999X".to_string(),
            ..Default::default()
        };
        assert!(Mt63Demod::new(p).is_err());
    }

    #[test]
    fn psk31_default_afc_on() {
        let p = Psk31DemodParams::default();
        assert!(p.afc);
        assert!(p.rx_freq_hz.abs() < 1e-6, "default rx_freq is 0 (AFC)");
    }
}
