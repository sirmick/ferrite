//! Runtime injection of a complex IQ [`DcBlock`] right after the source.
//!
//! Why: zero-IF / direct-conversion radios (HackRF, RTL-SDR, …) park a
//! permanent DC spike at the tuned centre from LO self-mixing + I/Q
//! offset. The honest fix is to remove the *artifact* once, at the IQ
//! stream, **before** the FFT tap splits — then it's gone from the
//! wideband waterfall, the narrow channel FFT, captures, and the
//! `SignalList` watchlist consistently. That's a cross-cutting concern
//! every hardware-fed preset wants, so we synthesise it at compose time
//! the same way [`inject_signal_list`](crate::inject_signal_list) and
//! [`inject_narrow_fft`](crate::inject_narrow_fft) synthesise their taps,
//! rather than authoring it into every preset.
//!
//! Shape — splice on the source's output so every consumer sees clean IQ:
//!
//! ```text
//!   before:  src.out ─► <consumer>            (tee / channelizer / fft …)
//!   after:   src.out ─► dcblock.in
//!            dcblock.out ─► <consumer>          (re-pointed, all of them)
//! ```
//!
//! **Hardware only.** We inject solely when the composed source is a
//! `SoapySource` — the synthetic sources (`SineSource`, `FileSource`,
//! `ModulatedFileSource`) used by the modulated-loopback fidelity e2e
//! tests have no DC spike, and a DC blocker on an AM-at-centre loopback
//! would null the carrier. Real zero-IF receivers always dodge DC, so on
//! hardware the blocker only ever removes the artifact, never programme.
//!
//! The block id is the bare `dcblock` (not `__`-prefixed) so the live
//! param surface stays stable: `param dcblock enabled=false` /
//! `corner_hz=…` toggle the spike removal in place (both are
//! `ReconfigureScope::SelfBlock`), no graph rebuild.
//!
//! Pass timing: AFTER `compose_source` (so `src` is the real source type)
//! and BEFORE `inject_narrow_fft` / `inject_signal_list` (so both FFT
//! taps and the watchlist read DC-blocked samples), which is BEFORE
//! `split_for_environment`.

use crate::compose::SOURCE_ID;
use crate::doc::{BlockInstanceDecl, FlowgraphDoc, Wire};

/// Block id for the synthesised DC blocker. Bare (param-stable).
const DCBLOCK_ID: &str = "dcblock";
/// Source types that carry a zero-IF DC spike worth removing. Only the
/// real hardware source qualifies — synthetic/replay sources don't.
const HARDWARE_SOURCE: &str = "SoapySource";

/// Splice a [`DcBlock`](ferrite_blocks::DcBlock) onto the source's IQ
/// output. No-op unless the composed source is a hardware `SoapySource`
/// with at least one consumer, and idempotent (skips if already present).
pub fn inject_dc_block_taps(doc: &mut FlowgraphDoc) {
    // Idempotent / opt-out: a preset that already wires a `dcblock` keeps
    // its own, and a re-compose won't double-insert.
    if doc.blocks.contains_key(DCBLOCK_ID) {
        return;
    }

    // Hardware sources only (see module docs).
    let is_hw = doc
        .blocks
        .get(SOURCE_ID)
        .is_some_and(|b| b.type_name == HARDWARE_SOURCE);
    if !is_hw {
        return;
    }

    let src_out = format!("{SOURCE_ID}.out");
    // Nothing wired off the source output → nothing to clean.
    if !doc.wires.iter().any(|w| w.src == src_out) {
        return;
    }

    // Match the source's placement so env_split keeps the blocker on the
    // same side (node, for a hardware source).
    let placement = doc.blocks.get(SOURCE_ID).and_then(|b| b.placement);

    // Re-point every consumer of src.out to dcblock.out, then feed the
    // blocker from src.out.
    let dcblock_out = format!("{DCBLOCK_ID}.out");
    for wire in &mut doc.wires {
        if wire.src == src_out {
            wire.src.clone_from(&dcblock_out);
        }
    }
    doc.wires
        .push(Wire::new(src_out, format!("{DCBLOCK_ID}.in")));

    doc.blocks.insert(
        DCBLOCK_ID.to_string(),
        BlockInstanceDecl {
            type_name: "DcBlock".into(),
            // Defaults: enabled, ~200 Hz corner. Operator toggles live.
            params: None,
            placement,
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Environment, FlowgraphDoc};

    fn parse(json: &[u8]) -> FlowgraphDoc {
        FlowgraphDoc::from_json(json).expect("doc parses")
    }

    /// A composed hardware preset: SoapySource → tee → (fft + chan).
    fn hardware() -> FlowgraphDoc {
        parse(
            br#"{
                "name": "hw",
                "environments": ["node","browser"],
                "blocks": {
                    "src":  {"type": "SoapySource", "placement": "node"},
                    "tee":  {"type": "TeeIqF32"},
                    "chan": {"type": "Channelizer"},
                    "fft":  {"type": "FFT"}
                },
                "wires": [
                    ["src.out", "tee.in"],
                    ["tee.out0", "chan.in"],
                    ["tee.out1", "fft.in"]
                ]
            }"#,
        )
    }

    #[test]
    fn injects_dc_block_after_hardware_source() {
        let mut doc = hardware();
        inject_dc_block_taps(&mut doc);

        let db = doc.blocks.get("dcblock").expect("dcblock injected");
        assert_eq!(db.type_name, "DcBlock");
        assert_eq!(db.placement, Some(Environment::Node));

        // src.out now feeds dcblock.in...
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "src.out" && w.dst == "dcblock.in"));
        // ...and the former consumer (tee.in) now comes from dcblock.out,
        // not src.out.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "dcblock.out" && w.dst == "tee.in"));
        assert!(!doc
            .wires
            .iter()
            .any(|w| w.src == "src.out" && w.dst == "tee.in"));
    }

    #[test]
    fn skips_synthetic_source() {
        // A modulated-loopback / replay source must not get a DC blocker.
        let mut doc = parse(
            br#"{
                "name": "loop",
                "environments": ["node"],
                "blocks": {
                    "src":  {"type": "ModulatedFileSource"},
                    "chan": {"type": "Channelizer"}
                },
                "wires": [["src.out", "chan.in"]]
            }"#,
        );
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_dc_block_taps(&mut doc);
        assert_eq!(doc.blocks.len(), b, "no block added for synthetic source");
        assert_eq!(doc.wires.len(), w);
        assert!(!doc.blocks.contains_key("dcblock"));
    }

    #[test]
    fn idempotent_on_double_inject() {
        let mut doc = hardware();
        inject_dc_block_taps(&mut doc);
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_dc_block_taps(&mut doc);
        assert_eq!(doc.blocks.len(), b);
        assert_eq!(doc.wires.len(), w);
    }

    #[test]
    fn fans_out_to_every_consumer() {
        // src.out feeding two consumers directly: both re-point.
        let mut doc = parse(
            br#"{
                "name": "fan",
                "environments": ["node"],
                "blocks": {
                    "src": {"type": "SoapySource", "placement": "node"},
                    "a":   {"type": "Channelizer"},
                    "b":   {"type": "FFT"}
                },
                "wires": [["src.out", "a.in"], ["src.out", "b.in"]]
            }"#,
        );
        inject_dc_block_taps(&mut doc);
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "dcblock.out" && w.dst == "a.in"));
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "dcblock.out" && w.dst == "b.in"));
        // Exactly one feed into the blocker.
        assert_eq!(
            doc.wires.iter().filter(|w| w.dst == "dcblock.in").count(),
            1
        );
    }
}
