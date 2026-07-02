//! Runtime injection of a `SignalList` tap on the wideband FFT byte stream.
//!
//! Why: the strongest-signal watchlist (the `signals` MCP verb + the UI's
//! "Strongest Signals" pane) is a cross-cutting tap that every preset with
//! a wideband waterfall should have — surveying a band shouldn't depend on
//! which preset happens to be loaded. Authoring it into each preset (it was,
//! by hand, in the 5 wideband presets) left every *other* preset — packet,
//! ft8, cw, ssb… — unable to survey, which is exactly the gap that bit an
//! APRS check on the `packet` preset. So we synthesise it at compose time,
//! the same way [`inject_narrow_fft`](crate::inject_narrow_fft) synthesises
//! the per-channel FFT.
//!
//! `SignalList` is an inline pass-through (`FftU8` in → `FftU8` out + an
//! `Events` out), so unlike the narrow-FFT tap it needs no `Tee` — it
//! splices directly onto the `ui:fft` wire:
//!
//! ```text
//!   before:  <logmag>.out ─► ui:fft
//!   after:   <logmag>.out ─► signals.in
//!            signals.out   ─► ui:fft        (waterfall preserved)
//!            signals.events ─► ui:signals   (the new watchlist sink)
//! ```
//!
//! The `LogMagU8` feeding `ui:fft` is the universal anchor: it's the byte
//! stream the waterfall renders, and it carries the source centre + rate
//! via port-meta propagation, so `SignalList` reports absolute RF
//! frequencies for free.
//!
//! Pass timing: AFTER `compose_source` / `inject_narrow_fft` (the doc has
//! its final wideband chain), BEFORE `split_for_environment` (so `env_split`
//! sees the new `ui:signals` sink, places the `NativeOnly` `SignalList`
//! node-side, and bridges the events to the browser — same as it does for
//! `ui:fft` today). No preset JSON edits needed.

use serde_json::json;

use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};

/// Block id for the synthesised detector. Plain `signals` (not `__`-prefixed)
/// so the live param surface stays stable — `param signals max_bw_hz=…`,
/// the `ui:signals` sink, and the frontend store all key off it.
const SIGNALS_ID: &str = "signals";
const UI_SIGNALS: &str = "ui:signals";
const UI_FFT: &str = "ui:fft";

/// Default `SignalList` frame size when the upstream `LogMagU8` doesn't pin
/// one — the wideband convention.
const DEFAULT_SIZE: u64 = 16_384;

/// Splice a `SignalList` onto the preset's wideband FFT (`ui:fft`) terminus,
/// surfacing a `ui:signals` watchlist sink. No-op for presets without a
/// wideband waterfall (bareband / decoder-only) or that already carry their
/// own `signals` wiring (opt-out / re-compose idempotency).
pub fn inject_signal_list_taps(doc: &mut FlowgraphDoc) {
    // Opt-out / idempotent: a preset that already has a `signals` block or
    // wires `ui:signals` keeps its own wiring, and a re-compose won't double.
    if doc.blocks.contains_key(SIGNALS_ID) || doc.wires.iter().any(|w| w.dst == UI_SIGNALS) {
        return;
    }

    // Find the wire feeding the wideband waterfall sink.
    let Some(wire_idx) = doc.wires.iter().position(|w| w.dst == UI_FFT) else {
        return; // no wideband FFT to tap
    };
    let producer = doc.wires[wire_idx]
        .src
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();

    // The producer must be a LogMagU8 (the FftU8 byte stream SignalList
    // reads). Guard against an odd preset whose ui:fft is fed by something
    // else — better to skip than to wire a type mismatch.
    let is_logmag = doc
        .blocks
        .get(&producer)
        .is_some_and(|b| b.type_name == "LogMagU8");
    if !is_logmag {
        return;
    }

    // Match SignalList's frame size to the upstream LogMagU8's `size`.
    let size = doc
        .blocks
        .get(&producer)
        .and_then(|b| b.params.as_ref())
        .and_then(|p| p.get("size"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_SIZE);

    // Re-point logmag.out → signals.in, then add the pass-through to ui:fft
    // and the watchlist sink.
    doc.wires[wire_idx].dst = format!("{SIGNALS_ID}.in");
    doc.wires
        .push(Wire::new(format!("{SIGNALS_ID}.out"), UI_FFT.to_string()));
    doc.wires.push(Wire::new(
        format!("{SIGNALS_ID}.events"),
        UI_SIGNALS.to_string(),
    ));

    // NativeOnly node-side block; env_split bridges ui:signals to the browser.
    doc.blocks.insert(
        SIGNALS_ID.to_string(),
        BlockInstanceDecl {
            type_name: "SignalList".into(),
            params: Some(json!({ "size": size })),
            placement: Some(Environment::Node),
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::FlowgraphDoc;

    fn parse(json: &[u8]) -> FlowgraphDoc {
        FlowgraphDoc::from_json(json).expect("doc parses")
    }

    /// A minimal preset with a wideband FFT chain (src → fft → logmag → ui:fft).
    fn wideband() -> FlowgraphDoc {
        parse(
            br#"{
                "name": "wb",
                "environments": ["node","browser"],
                "blocks": {
                    "src":    {"type": "SineSource"},
                    "fft":    {"type": "FFT", "params": {"size": 8192}},
                    "logmag": {"type": "LogMagU8", "params": {"size": 8192}}
                },
                "wires": [
                    ["src.out", "fft.in"],
                    ["fft.out", "logmag.in"],
                    ["logmag.out", "ui:fft"]
                ]
            }"#,
        )
    }

    #[test]
    fn injects_signal_list_inline_on_ui_fft() {
        let mut doc = wideband();
        inject_signal_list_taps(&mut doc);

        // The SignalList block was materialised, node-side, size matched.
        let sig = doc.blocks.get("signals").expect("signals block injected");
        assert_eq!(sig.type_name, "SignalList");
        assert_eq!(sig.placement, Some(Environment::Node));
        assert_eq!(
            sig.params.as_ref().unwrap().get("size").unwrap().as_u64(),
            Some(8192),
            "size matched the upstream LogMagU8"
        );

        // logmag.out now feeds signals.in (re-pointed), not ui:fft directly.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "logmag.out" && w.dst == "signals.in"));
        assert!(!doc
            .wires
            .iter()
            .any(|w| w.src == "logmag.out" && w.dst == "ui:fft"));
        // Pass-through preserves the waterfall, and the watchlist sink exists.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "signals.out" && w.dst == "ui:fft"));
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "signals.events" && w.dst == "ui:signals"));
    }

    #[test]
    fn no_ui_fft_means_no_injection() {
        // A decoder-only preset with no wideband waterfall.
        let mut doc = parse(
            br#"{
                "name": "bare",
                "environments": ["node"],
                "blocks": {"src": {"type": "SineSource"}, "sink": {"type": "FileIqSink"}},
                "wires": [["src.out", "sink.in"]]
            }"#,
        );
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_signal_list_taps(&mut doc);
        assert_eq!(doc.blocks.len(), b);
        assert_eq!(doc.wires.len(), w);
    }

    #[test]
    fn idempotent_on_double_inject() {
        let mut doc = wideband();
        inject_signal_list_taps(&mut doc);
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_signal_list_taps(&mut doc);
        assert_eq!(doc.blocks.len(), b);
        assert_eq!(doc.wires.len(), w);
    }

    #[test]
    fn skips_when_preset_already_wires_signals() {
        // A preset that hand-wired its own signals keeps it untouched.
        let mut doc = wideband();
        doc.blocks.insert(
            "signals".into(),
            BlockInstanceDecl {
                type_name: "SignalList".into(),
                ..Default::default()
            },
        );
        let b = doc.blocks.len();
        inject_signal_list_taps(&mut doc);
        assert_eq!(doc.blocks.len(), b, "did not re-inject over existing");
    }

    #[test]
    fn ui_fft_fed_by_non_logmag_is_skipped() {
        // Defensive: ui:fft fed by something that isn't a LogMagU8.
        let mut doc = parse(
            br#"{
                "name": "weird",
                "environments": ["node"],
                "blocks": {"src": {"type": "SineSource"}},
                "wires": [["src.out", "ui:fft"]]
            }"#,
        );
        inject_signal_list_taps(&mut doc);
        assert!(!doc.blocks.contains_key("signals"));
    }
}
