//! Runtime injection of a `DcBlocker` block between the source and the
//! channelizer for zero-IF SDR users who tune on-target.
//!
//! Why: HackRF (zero-IF across its entire 1 MHz–6 GHz range) and
//! SDRplay (zero-IF above ~30 MHz) leak local-oscillator energy onto
//! the ADC, producing a constant bright spike at the tuned centre
//! frequency. When the operator parks the source on a target signal,
//! the spike obliterates it and the auto-contrast waterfall washes out
//! whatever else is on the band.
//!
//! The standard SDR workaround is the off-tune-and-VFO-shift pattern
//! (source tuned 50–100 kHz off-target, channelizer's `freq_shift_hz`
//! pulls the actual target back to baseband). That works but it's
//! fiddly and the AI has to remember to do it on every retune. The
//! cleaner fix is a single-pole high-pass at DC in the source IQ
//! stream — the `DcBlocker` block in `blocks/src/dc_blocker.rs`.
//!
//! Rather than authoring `dc_block` into every preset's JSON (~20
//! files at last count, every one with a Channelizer would need the
//! same dc_block→chan rewiring), we synthesise it at compose time —
//! same pattern as `inject_narrow_fft.rs`. Triggered by the runtime
//! `Profile.dc_block` toggle: when on (default), every channelizer in
//! the doc gets a DcBlocker hung upstream of its `in` port. When off,
//! injection is skipped and the operator's preset wiring is unchanged.
//!
//! Topology before injection:
//!
//! ```text
//!   <upstream>.out ─► chan.in
//! ```
//!
//! After:
//!
//! ```text
//!   <upstream>.out ─► __dc_block_<chan>.in
//!   __dc_block_<chan>.out ─► chan.in
//! ```
//!
//! No tee, no fork — the DC blocker is a pass-through filter on the
//! channelizer's only input. Multi-channelizer presets (rare) get one
//! DcBlocker per channelizer with a unique id.
//!
//! Idempotent: if the doc already contains a block whose id starts
//! with `__dc_block`, the pass is a no-op (assume a prior compose
//! cycle already ran).

use serde_json::{json, Map, Value};

use crate::apply_profile::Profile;
use crate::doc::{BlockInstanceDecl, FlowgraphDoc, Wire};

const PREFIX: &str = "__dc_block";

/// Inject a DcBlocker upstream of every Channelizer in `doc`, gated by
/// `profile.dc_block`. Idempotent and safe to call multiple times in a
/// reconfigure cycle.
pub fn inject_dc_blocker(doc: &mut FlowgraphDoc, profile: &Profile) {
    if !profile.dc_block {
        return;
    }
    if doc.blocks.keys().any(|k| k.starts_with(PREFIX)) {
        // Already injected (e.g. by a prior pass).
        return;
    }
    let channelizer_ids: Vec<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| b.type_name == "Channelizer")
        .map(|(id, _)| id.clone())
        .collect();
    if channelizer_ids.is_empty() {
        return;
    }
    for chan_id in channelizer_ids {
        inject_for_channelizer(doc, &chan_id);
    }
}

fn inject_for_channelizer(doc: &mut FlowgraphDoc, chan_id: &str) {
    let dc_id = format!("{PREFIX}_{chan_id}");
    if doc.blocks.contains_key(&dc_id) {
        return;
    }

    // Find every wire that lands on this channelizer's input port and
    // redirect it through the new DcBlocker. There's typically exactly
    // one — but a presets-with-multiple-feeds-to-one-chan scenario
    // would land here too, and either way the rewiring is correct: all
    // upstream feeds now flow through the DC blocker, only one wire
    // from `dc_block.out → chan.in` lands on the channelizer.
    let chan_in = format!("{chan_id}.in");
    let dc_in = format!("{dc_id}.in");
    let mut redirected = 0;
    for wire in doc.wires.iter_mut() {
        if wire.dst == chan_in {
            wire.dst = dc_in.clone();
            redirected += 1;
        }
    }
    if redirected == 0 {
        // Channelizer has no input wire yet — nothing to inject around.
        // Could happen during partial-compose; bail without polluting
        // the doc.
        return;
    }
    doc.wires.push(Wire::new(format!("{dc_id}.out"), chan_in));

    // Materialise the new DcBlocker block alongside the channelizer.
    // Inherits the channelizer's placement so the bridge crossing
    // (env_split-inserted) doesn't reshape around it — DC blocking is
    // single-rate IQ→IQ, zero-cross-env value to splitting it off.
    let placement = doc.blocks.get(chan_id).and_then(|b| b.placement);
    let mut params = Map::new();
    params.insert("pole".to_string(), json!(0.995));
    doc.blocks.insert(
        dc_id,
        BlockInstanceDecl {
            type_name: "DcBlocker".to_string(),
            placement,
            placement_role: None,
            when: None,
            params: Some(Value::Object(params)),
            force_params: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc_with_chan() -> FlowgraphDoc {
        serde_json::from_value(json!({
            "name": "t",
            "environments": ["node"],
            "blocks": {
                "src": { "type": "SoapySource", "placement": "node" },
                "chan": {
                    "type": "Channelizer",
                    "placement": "node",
                    "params": { "output_rate_hz": 250000.0 }
                }
            },
            "wires": [["src.out", "chan.in"]]
        }))
        .unwrap()
    }

    #[test]
    fn injects_when_profile_enabled() {
        let mut doc = doc_with_chan();
        inject_dc_blocker(&mut doc, &Profile::default());
        assert!(doc.blocks.contains_key("__dc_block_chan"));
        // Original src→chan wire now goes src→dc, plus dc→chan.
        let src_to_dc = doc
            .wires
            .iter()
            .find(|w| w.src == "src.out" && w.dst == "__dc_block_chan.in");
        let dc_to_chan = doc
            .wires
            .iter()
            .find(|w| w.src == "__dc_block_chan.out" && w.dst == "chan.in");
        assert!(src_to_dc.is_some(), "src→dc wire missing");
        assert!(dc_to_chan.is_some(), "dc→chan wire missing");
        // No stale direct src→chan wire.
        assert!(!doc
            .wires
            .iter()
            .any(|w| w.src == "src.out" && w.dst == "chan.in"));
    }

    #[test]
    fn skips_when_profile_disabled() {
        let mut doc = doc_with_chan();
        let profile = Profile {
            dc_block: false,
            ..Profile::default()
        };
        inject_dc_blocker(&mut doc, &profile);
        assert!(!doc.blocks.keys().any(|k| k.starts_with(PREFIX)));
        // Original wiring preserved.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "src.out" && w.dst == "chan.in"));
    }

    #[test]
    fn skips_without_channelizer() {
        let mut doc: FlowgraphDoc = serde_json::from_value(json!({
            "name": "t",
            "environments": ["node"],
            "blocks": {
                "src": { "type": "SoapySource", "placement": "node" },
                "fft": { "type": "FFT", "placement": "node" }
            },
            "wires": [["src.out", "fft.in"]]
        }))
        .unwrap();
        inject_dc_blocker(&mut doc, &Profile::default());
        assert!(!doc.blocks.keys().any(|k| k.starts_with(PREFIX)));
    }

    #[test]
    fn idempotent_under_repeated_call() {
        use std::collections::BTreeMap;
        let mut doc = doc_with_chan();
        inject_dc_blocker(&mut doc, &Profile::default());
        let snapshot: BTreeMap<_, _> = doc.blocks.clone().into_iter().collect();
        let wires_snapshot = doc.wires.clone();
        inject_dc_blocker(&mut doc, &Profile::default());
        let after: BTreeMap<_, _> = doc.blocks.clone().into_iter().collect();
        assert_eq!(
            snapshot.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>()
        );
        assert_eq!(doc.wires, wires_snapshot);
    }
}
