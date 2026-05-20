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
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
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
    /// UTF-8 newline-delimited JSON decode records queued for the
    /// `events` port — the same transport ft8/RDS/DTMF use, so the
    /// decoded text is observable browser-side (`drainEvents`) and
    /// node-side (`ui:events`), not only via the `decoder::fldigi`
    /// tracing log.
    events_out: Vec<u8>,
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
            events_out: Vec::new(),
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

    /// TEST-ONLY: queue an RSID detection (see
    /// [`ferrite_fldigi::FldigiModem::inject_rsid`]).
    pub fn inject_rsid(&mut self, id: &str) {
        self.modem.inject_rsid(id);
    }

    /// The current mode label (`decoder::fldigi mode=` field).
    #[must_use]
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
            // Mirror onto the `events` port as one newline-terminated
            // JSON record (serde_json handles escaping). `t:"fldigi"`
            // tags the family for the generic decode console; `mode`
            // is the variant label. No timestamp: fldigi text has no
            // slot time and `SystemTime::now()` panics on
            // wasm32-unknown-unknown — the consumer stamps on receipt.
            let rec = serde_json::json!({
                "t": "fldigi",
                "mode": self.label,
                "text": text,
            });
            if serde_json::to_writer(&mut self.events_out, &rec).is_ok() {
                self.events_out.push(b'\n');
            }
        }
        // Scope: future tuning-aid widget; status: fldigi UI chrome.
        // Drain both so they don't grow unbounded.
        let _ = self.modem.take_scope();
        let _ = self.modem.take_status();

        // Drain queued JSON records into the `events` port (same
        // pattern as Ft8Demod). Any remainder that doesn't fit the
        // ring this tick stays buffered for the next — audio flows
        // continuously so `pump` is called again soon.
        if !self.events_out.is_empty() {
            for port in io.outputs.iter_mut() {
                if port.name == "events" {
                    if let OutBuf::Events(dst) = &mut port.buf {
                        let take = self.events_out.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.events_out[..take]);
                            self.events_out.drain(..take);
                            work.produced[0] = take;
                        }
                    }
                }
            }
        }
        work
    }
}

/// Real-audio input port shared by every fldigi block (8 kHz mono).
const FLDIGI_IN: &[PortSpec] = &[PortSpec {
    name: "in",
    port_type: PortType::RealF32,
}];

/// Decoded-text egress shared by every fldigi block: newline-delimited
/// JSON on a `PortType::Events` port (same transport as ft8/RDS/DTMF).
/// Wire it to `ui:events`/`EventsSink`; unwired it's simply ignored,
/// and `decoder::fldigi` tracing still carries the text for
/// `tail decoder`.
const FLDIGI_OUT: &[PortSpec] = &[PortSpec {
    name: "events",
    port_type: PortType::Events,
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
// fldigi_family! — the shared scaffold every per-mode block follows.
//
// Generates: Params struct (with the family's real params plus the
// always-on `afc`/`rx_freq_hz`/`reverse`), Default, the Block impl
// (spec/init/process/apply_live_params), and the BlockFactory impl.
// Per-family knobs: extra ParamSpecs and a `compose: |p| -> Result<id>`
// closure that picks the fldigi make_modem id from the real params.
// `post_init:` is an optional second closure for families whose modem
// uses config TAGs after construction (RTTY's index-based
// RTTYBAUD/RTTYSHIFT).
//
// Reverse is carried unconditionally — apply_reverse just sets the
// `rtty_reverse` modem param, which is a no-op for BPSK/DBPSK/CW
// modems (those families simply omit REVERSE_PARAM from their spec so
// it isn't surfaced as a UI control).
// ---------------------------------------------------------------------

macro_rules! fldigi_family {
    (
        $params:ident, $block:ident, $type_name:literal,
        fields { $($field:ident : $fty:ty = $fdefault:expr),* $(,)? },
        params_extra: $extra:expr,
        compose: $compose:expr,
        $(post_init: $post_init:expr,)?
        ai: $ai:literal $(,)?
    ) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(default)]
        pub struct $params {
            $(pub $field: $fty,)*
            pub afc: bool,
            pub rx_freq_hz: f32,
            pub reverse: bool,
        }
        impl Default for $params {
            fn default() -> Self {
                Self {
                    $($field: $fdefault,)*
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
                let composer: fn(&$params) -> Result<String> = $compose;
                let id = composer(&p)?;
                let mut core = FldigiCore::new(&id, id.clone())?;
                core.apply_common(p.afc, p.rx_freq_hz);
                core.apply_reverse(p.reverse);
                $(
                    let post: fn(&$params, &mut FldigiCore) -> Result<()> = $post_init;
                    post(&p, &mut core)?;
                )?
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
                    outputs: FLDIGI_OUT,
                    params: $extra,
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

// ---------------------------------------------------------------------
// RttyDemod — Baudot/ITA2 RTTY. `baud` + `shift` are exposed as real
// numeric params (Bd / Hz); presets pin the common combos (docs/14:
// expose the knobs, ship presets for the usual values — supersedes the
// block-owned-taxonomy idea).
//
// fldigi's API is index-based: `rtty45/50/75/100` all build the one
// `MODE_RTTY` modem, and `RTTYBAUD`/`RTTYSHIFT` are *indices* into
// fixed vendored tables (`rtty.cxx`), not Hz. So the block does a thin
// value→index adapter against mirrors of those tables — that's a
// mechanical translation, not a curated taxonomy.
// ---------------------------------------------------------------------

/// Mirror of `rtty::BAUD[]` (rtty.cxx) — `RTTYBAUD` is an index here.
const RTTY_BAUD_TABLE: &[f64] = &[
    45.0, 45.45, 50.0, 56.0, 75.0, 100.0, 110.0, 150.0, 200.0, 300.0,
];
/// Mirror of `rtty::SHIFT[]` (rtty.cxx) — `RTTYSHIFT` is an index here.
const RTTY_SHIFT_TABLE: &[f64] = &[
    23.0, 85.0, 160.0, 170.0, 182.0, 200.0, 240.0, 350.0, 425.0, 850.0,
];

/// Baud values surfaced as the `baud` enum (the on-air-real subset).
const RTTY_BAUDS: &[f64] = &[45.45, 50.0, 75.0, 100.0];
/// Shift values surfaced as the `shift` enum.
const RTTY_SHIFTS: &[f64] = &[170.0, 425.0, 850.0];

/// Find the fldigi table index for `v` (exact-ish match — these are
/// fixed catalog values, not measured). Errors list the valid set.
fn table_index(table: &[f64], v: f64, what: &str) -> Result<i32> {
    table
        .iter()
        .position(|t| (t - v).abs() < 0.01)
        .map(|i| i32::try_from(i).unwrap_or(0))
        .ok_or_else(|| anyhow::anyhow!("RttyDemod: unsupported {what} {v}"))
}

fldigi_family!(
    RttyDemodParams, RttyDemod, "RttyDemod",
    fields { baud: f64 = 45.45, shift: f64 = 170.0 },
    params_extra: &[
        ParamSpec { key: "baud", label: "Baud",
            kind: ParamKind::EnumNumeric { values: RTTY_BAUDS, default: 45.45, unit: "Bd" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Symbol rate. 45.45 Bd is classic amateur Baudot RTTY; 50/75/100 are commercial/military." },
        ParamSpec { key: "shift", label: "Shift",
            kind: ParamKind::EnumNumeric { values: RTTY_SHIFTS, default: 170.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Mark/space frequency separation. 170 Hz is amateur standard; 425/850 are commercial." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    // All RTTY baud/shift combos share the one `rtty` modem; the
    // selectors live in fldigi's index-based RTTYBAUD/RTTYSHIFT TAGs.
    compose: |_| Ok("rtty".to_string()),
    post_init: |p, core| {
        let baud_idx = table_index(RTTY_BAUD_TABLE, p.baud, "baud")?;
        let shift_idx = table_index(RTTY_SHIFT_TABLE, p.shift, "shift")?;
        core.set("RTTYBAUD", f64::from(baud_idx));
        core.set("RTTYSHIFT", f64::from(shift_idx));
        Ok(())
    },
    ai: "RTTY (Baudot/ITA2) via the curated fldigi rtty core; `baud`+`shift` are real params (45.45 Bd / 170 Hz = amateur standard). Tune so the two tones straddle the audio carrier; `reverse` if inverted. Output: `tail decoder --category fldigi`.",
);

// ---------------------------------------------------------------------
// Psk31Demod — BPSK, `baud` selectable (31 / 63 / 125). Each baud is a
// distinct fldigi make_modem id (Kind-1): the block composes the id
// from the real `baud` param; presets pin the common ones (docs/14).
// Type name kept `Psk31Demod` for preset/wire back-compat even though
// it now covers 63/125.
// ---------------------------------------------------------------------

/// Surfaced `baud` values; each maps 1:1 to a `make_modem` id.
const PSK_BAUDS: &[f64] = &[31.0, 63.0, 125.0];

fldigi_family!(
    Psk31DemodParams, Psk31Demod, "Psk31Demod",
    fields { baud: f64 = 31.0 },
    params_extra: &[
        ParamSpec { key: "baud", label: "Baud",
            kind: ParamKind::EnumNumeric { values: PSK_BAUDS, default: 31.0, unit: "Bd" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "BPSK rate: 31 (keyboard chat), 63/125 (faster, wider)." },
        AFC_PARAM, RX_FREQ_PARAM,
    ],
    // psk31 / psk63 / psk125 are distinct make_modem ids; BPSK is
    // sideband-symmetric so `reverse` (carried by the macro) is a no-op
    // for the modem and isn't surfaced as a UI control.
    compose: |p| match p.baud as i64 {
        31 => Ok("psk31".to_string()),
        63 => Ok("psk63".to_string()),
        125 => Ok("psk125".to_string()),
        _ => bail!("Psk31Demod: baud must be one of {PSK_BAUDS:?}, got {}", p.baud),
    },
    ai: "BPSK (31/63/125) via the curated fldigi psk core. Narrow, AFC pulls it in. Output: `tail decoder --category fldigi`.",
);

// ---------------------------------------------------------------------
// CwDemod — Morse via the curated fldigi cw core (the `_cw_live`
// preset's decoder; distinct from the multimon `MorseDemod`).
// ---------------------------------------------------------------------

// CwDemod has no per-family params beyond AFC + RX carrier; the macro
// still adds a `reverse` field for uniformity (Morse polarity has no
// meaning so it's not surfaced in the spec — see Psk31Demod).
fldigi_family!(
    CwDemodParams, CwDemod, "CwDemod",
    fields {},
    params_extra: &[AFC_PARAM, RX_FREQ_PARAM],
    compose: |_| Ok("cw".to_string()),
    ai: "Morse/CW via the curated fldigi cw core (decode-side; the multimon MorseDemod is the other CW path). Output: `tail decoder --category fldigi`.",
);

// ---------------------------------------------------------------------
// FldigiAuto — RSID-driven automatic mode. One block; RSID runs
// alongside the active modem and, on a Reed-Solomon mode-ID, rebuilds
// the *inner* FldigiCore to the detected mode. Block type/ports/wiring
// never change — the swap is a private state transition (the fldigi
// analogue of multimon's adaptive single block). Use this when the
// mode is unknown; the per-mode blocks remain for known-mode presets.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FldigiAutoParams {
    /// Mode to decode until RSID says otherwise (a `make_modem` id).
    pub start_mode: String,
    pub afc: bool,
    pub rx_freq_hz: f32,
}

impl Default for FldigiAutoParams {
    fn default() -> Self {
        Self {
            start_mode: "rtty45".to_string(),
            afc: true,
            rx_freq_hz: 0.0,
        }
    }
}

pub struct FldigiAuto {
    core: FldigiCore,
    afc: bool,
    rx_freq_hz: f32,
}

impl FldigiAuto {
    pub fn new(p: FldigiAutoParams) -> Result<Self> {
        let mut core = FldigiCore::new(&p.start_mode, p.start_mode.clone())?;
        core.apply_common(p.afc, p.rx_freq_hz);
        core.set("RECEIVERSID", 1.0); // per-handle: shim creates cRsId
        Ok(Self {
            core,
            afc: p.afc,
            rx_freq_hz: p.rx_freq_hz,
        })
    }

    /// Re-arm a freshly rebuilt modem: same front-end + RSID still on,
    /// so further mode changes keep being detected.
    fn rearm(&mut self) {
        self.core.apply_common(self.afc, self.rx_freq_hz);
        self.core.set("RECEIVERSID", 1.0);
    }

    /// TEST-ONLY: simulate an RSID detection of `id`; the next
    /// `process()` acts on it (switch + decode). Exercises our
    /// detection→switch path without fldigi's RS encoder.
    pub fn inject_rsid(&mut self, id: &str) {
        self.core.inject_rsid(id);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FldigiAuto {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FldigiAuto",
            placement: Placement::Either,
            inputs: FLDIGI_IN,
            outputs: FLDIGI_OUT,
            params: &[
                ParamSpec {
                    key: "start_mode",
                    label: "Start mode",
                    kind: ParamKind::Text { default: "rtty45" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Mode decoded until an RSID burst is seen (a make_modem id, e.g. rtty45/psk31/olivia-8-500). RSID then switches it automatically.",
                },
                AFC_PARAM,
                RX_FREQ_PARAM,
            ],
            ai_notes: "Auto digital-mode decoder: fldigi RSID identifies the mode in-band and FldigiAuto rebuilds its internal modem to match — no preset/graph change. Use when the mode is unknown; per-mode blocks (RttyDemod, …) are for known-mode presets. Output: `tail decoder --category fldigi`; switches logged to decoder::rsid.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.core.warn_if_off_rate(ctx);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Feed the chunk: the shim hands the same audio to the active
        // modem AND the RSID detector.
        let work = self.core.pump(io);
        // Act on any RSID hit (last wins within this chunk).
        if let Some(detected) = self.core.take_rsid().into_iter().last() {
            if detected != self.core.label() {
                let from = self.core.label().to_string();
                if let Err(e) = self.core.switch_mode(&detected, detected.clone()) {
                    tracing::warn!(target: "decoder::rsid", %detected, "RSID switch failed: {e}");
                } else {
                    self.rearm();
                    tracing::info!(target: "decoder::rsid", from = %from, to = %detected, "RSID");
                }
            }
        }
        Ok(work)
    }
}

impl BlockFactory for FldigiAuto {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(FldigiAuto::new(
            crate::block::deserialize_params(params)?,
        )?))
    }
}

// ---------------------------------------------------------------------
// Mt63Demod — MT63, variant-selected (64-tone DBPSK + interleave/FEC).
// ---------------------------------------------------------------------

/// MT63 bandwidths (Hz) surfaced as the `bandwidth` enum.
const MT63_BANDWIDTHS: &[f64] = &[500.0, 1000.0, 2000.0];
/// Interleave depth — fldigi's `S`(hort) / `L`(ong) id suffix.
const MT63_INTERLEAVES: &[&str] = &["short", "long"];

fldigi_family!(
    Mt63DemodParams, Mt63Demod, "Mt63Demod",
    fields { bandwidth: f64 = 1000.0, interleave: String = "long".to_string() },
    params_extra: &[
        ParamSpec { key: "bandwidth", label: "Bandwidth",
            kind: ParamKind::EnumNumeric { values: MT63_BANDWIDTHS, default: 1000.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Signal width. 1000 Hz is the EmComm/MARS workhorse." },
        ParamSpec { key: "interleave", label: "Interleave",
            kind: ParamKind::EnumString { values: MT63_INTERLEAVES, default: "long" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Interleaver depth. long = more robust + more latency (the net default); short = faster." },
        AFC_PARAM, RX_FREQ_PARAM,
    ],
    // Each (bandwidth, interleave) is a distinct make_modem id
    // (Kind-1) — compose `mt63-{bw}{S|L}` from the real params.
    compose: |p| {
        let bw = match p.bandwidth as i64 {
            500 | 1000 | 2000 => p.bandwidth as i64,
            _ => bail!("Mt63Demod: bandwidth must be one of {MT63_BANDWIDTHS:?}, got {}", p.bandwidth),
        };
        let suffix = match p.interleave.as_str() {
            "short" => "S",
            "long" => "L",
            _ => bail!("Mt63Demod: interleave must be one of {MT63_INTERLEAVES:?}, got {:?}", p.interleave),
        };
        Ok(format!("mt63-{bw}{suffix}"))
    },
    ai: "MT63 (64-tone DBPSK, interleave+FEC) via the curated fldigi mt63 core. Several-second decode latency by design. Tune the block ~1500 Hz if AFC can't find it. Output: `tail decoder --category fldigi`.",
);

// Olivia / Contestia: `tones` × `bandwidth`, composed to the discrete
// `{prefix}-{tones}-{bw}` make_modem id. Surfaced value sets are the
// combos that actually have a discrete id (no generic-mode fallback,
// so decode == the pre-existing path).
const OLIVIA_TONES: &[f64] = &[8.0, 16.0, 32.0];
const OLIVIA_BW: &[f64] = &[500.0, 1000.0];
const CONTESTIA_TONES: &[f64] = &[8.0, 16.0];
const CONTESTIA_BW: &[f64] = &[250.0, 500.0];

fn tones_bw_id(prefix: &str, allowed: &[&str], tones: f64, bw: f64) -> Result<String> {
    let id = format!("{prefix}-{}-{}", tones as i64, bw as i64);
    if allowed.contains(&id.as_str()) {
        Ok(id)
    } else {
        bail!("{prefix}: unsupported tones/bandwidth combo {tones}/{bw} (valid: {allowed:?})")
    }
}

fldigi_family!(
    OliviaDemodParams, OliviaDemod, "OliviaDemod",
    fields { tones: f64 = 8.0, bandwidth: f64 = 500.0 },
    params_extra: &[
        ParamSpec { key: "tones", label: "Tones",
            kind: ParamKind::EnumNumeric { values: OLIVIA_TONES, default: 8.0, unit: "" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "MFSK tone count. More tones = more robust, slower." },
        ParamSpec { key: "bandwidth", label: "Bandwidth",
            kind: ParamKind::EnumNumeric { values: OLIVIA_BW, default: 500.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Signal width. 8/500 is the common amateur Olivia." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    compose: |p| tones_bw_id("olivia",
        &["olivia-8-500", "olivia-16-500", "olivia-32-1000"], p.tones, p.bandwidth),
    ai: "Olivia (MFSK + Reed-Solomon FEC) via the curated fldigi olivia core. Very robust, slow. Tolerant of mistuning; AFC usually pulls it in. Output: `tail decoder --category fldigi`.",
);
fldigi_family!(
    ContestiaDemodParams, ContestiaDemod, "ContestiaDemod",
    fields { tones: f64 = 8.0, bandwidth: f64 = 500.0 },
    params_extra: &[
        ParamSpec { key: "tones", label: "Tones",
            kind: ParamKind::EnumNumeric { values: CONTESTIA_TONES, default: 8.0, unit: "" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "MFSK tone count." },
        ParamSpec { key: "bandwidth", label: "Bandwidth",
            kind: ParamKind::EnumNumeric { values: CONTESTIA_BW, default: 500.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Signal width. 8/500 is the common amateur Contestia." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    compose: |p| tones_bw_id("contestia",
        &["contestia-8-250", "contestia-8-500", "contestia-16-500"], p.tones, p.bandwidth),
    ai: "Contestia (Olivia-derivative MFSK+FEC) via the curated fldigi contestia core. Tolerant of mistuning. Output: `tail decoder --category fldigi`.",
);

const DOMINOEX_SPEEDS: &[f64] = &[4.0, 8.0, 11.0, 16.0, 22.0, 44.0];
fldigi_family!(
    DominoexDemodParams, DominoexDemod, "DominoexDemod",
    fields { speed: f64 = 16.0 },
    params_extra: &[
        ParamSpec { key: "speed", label: "Speed",
            kind: ParamKind::EnumNumeric { values: DOMINOEX_SPEEDS, default: 16.0, unit: "" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "DominoEX speed code (baud class). 16 is the common HF setting." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    compose: |p| {
        if DOMINOEX_SPEEDS.contains(&p.speed) {
            Ok(format!("dominoex{}", p.speed as i64))
        } else {
            bail!("DominoexDemod: speed must be one of {DOMINOEX_SPEEDS:?}, got {}", p.speed)
        }
    },
    ai: "DominoEX (incremental-frequency MFSK) via the curated fldigi dominoex core. Sideband-sensitive — try `reverse` if garbled. Output: `tail decoder --category fldigi`.",
);

const THROB_TONES: &[f64] = &[1.0, 2.0, 4.0];
fldigi_family!(
    ThrobDemodParams, ThrobDemod, "ThrobDemod",
    fields { tones: f64 = 4.0, extended: bool = false },
    params_extra: &[
        ParamSpec { key: "tones", label: "Tones",
            kind: ParamKind::EnumNumeric { values: THROB_TONES, default: 4.0, unit: "" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Throb tone count (1/2/4)." },
        ParamSpec { key: "extended", label: "ThrobX",
            kind: ParamKind::Toggle { default: false },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "Use the ThrobX variant (wider, faster) instead of plain Throb." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    compose: |p| {
        if THROB_TONES.contains(&p.tones) {
            Ok(format!("throb{}{}", if p.extended { "x" } else { "" }, p.tones as i64))
        } else {
            bail!("ThrobDemod: tones must be one of {THROB_TONES:?}, got {}", p.tones)
        }
    },
    ai: "Throb / ThrobX (slow MFSK) via the curated fldigi throb core. Output: `tail decoder --category fldigi`.",
);

const NAVTEX_STANDARDS: &[&str] = &["navtex", "sitorb"];
fldigi_family!(
    NavtexDemodParams, NavtexDemod, "NavtexDemod",
    fields { standard: String = "navtex".to_string() },
    params_extra: &[
        ParamSpec { key: "standard", label: "Standard",
            kind: ParamKind::EnumString { values: NAVTEX_STANDARDS, default: "navtex" },
            reconfig_scope: ReconfigureScope::SourceRestart,
            ai_notes: "navtex (518 kHz maritime safety broadcast) or sitorb (SITOR-B / CCIR-476 ARQ-B)." },
        AFC_PARAM, RX_FREQ_PARAM, REVERSE_PARAM,
    ],
    compose: |p| {
        if NAVTEX_STANDARDS.contains(&p.standard.as_str()) {
            Ok(p.standard.clone())
        } else {
            bail!("NavtexDemod: standard must be one of {NAVTEX_STANDARDS:?}, got {:?}", p.standard)
        }
    },
    ai: "NAVTEX / SITOR-B (CCIR-476, 100 bd / 170 Hz shift, time-diversity FEC) via the curated fldigi navtex core. 518 kHz maritime safety. Output: `tail decoder --category fldigi`.",
);

#[cfg(test)]
mod tests {
    use super::{
        Mt63Demod, Mt63DemodParams, Psk31Demod, Psk31DemodParams, RttyDemod, RttyDemodParams,
    };
    use crate::block::Block;

    #[test]
    fn specs_are_real_in_events_out() {
        for (name, s) in [
            ("RttyDemod", RttyDemod::spec()),
            ("Psk31Demod", Psk31Demod::spec()),
            ("Mt63Demod", Mt63Demod::spec()),
        ] {
            assert_eq!(s.type_name, name);
            assert_eq!(s.inputs.len(), 1);
            assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
            // Decoded text now egresses on a single `events` port
            // (browser `drainEvents` / node `ui:events`).
            assert_eq!(s.outputs.len(), 1);
            assert_eq!(s.outputs[0].name, "events");
            assert_eq!(s.outputs[0].port_type, crate::block::PortType::Events);
        }
    }

    #[test]
    fn defaults_construct() {
        let _ = RttyDemod::new(RttyDemodParams::default()).unwrap();
        let _ = Psk31Demod::new(Psk31DemodParams::default()).unwrap();
        let _ = Mt63Demod::new(Mt63DemodParams::default()).unwrap();
    }

    #[test]
    fn mt63_rejects_unknown_bandwidth() {
        let p = Mt63DemodParams {
            bandwidth: 9999.0,
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

    #[test]
    fn rtty_rejects_off_table_values() {
        let p = super::RttyDemodParams {
            baud: 33.0,
            ..Default::default()
        };
        assert!(super::RttyDemod::new(p).is_err());
    }
}
