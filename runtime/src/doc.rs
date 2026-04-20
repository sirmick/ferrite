//! Flowgraph JSON document — serde shape.
//!
//! Mirrors `FlowgraphDoc` in `packages/flowgraph-runtime/src/types.ts`
//! byte-for-byte so the same JSON file parses on either side. Field
//! names stay camelCase-free (the JSON already uses snake-ish lowercase
//! keys like `environments`, `blocks`, `wires`) — serde's default
//! matches the TS shape without any `#[serde(rename)]` on this struct.

use serde::{
    de::{Error as _, SeqAccess, Visitor},
    ser::SerializeSeq,
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::collections::BTreeMap;
use std::fmt;

// Wire is serialized as a JSON array `[src, dst]` for TS/JSON
// compatibility with the existing preset files, but the Rust type is a
// struct so call sites can use named field access and grow the type
// later without touching every consumer.

/// Where a flowgraph (or half of one) runs.
///
/// `Browser` = WASM runtime in the browser. `Node` = native runtime
/// (today that's `ferrited`; the name is historical and tracks the TS
/// enum value for JSON compatibility — a rename to `native` is a
/// migration we'll do when both sides update together).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Browser,
    Node,
}

/// One block instance inside a flowgraph doc. `type` names a block type
/// in the registry; `params` is whatever JSON the block's `ParamSpec`
/// accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInstanceDecl {
    /// Type name, matches a `BlockSpec::type_name` in the registry.
    #[serde(rename = "type")]
    pub type_name: String,
    /// Raw params blob — deserialised per-block at instantiation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// Per-instance environment override. Pins a block with
    /// [`Placement::Either`](ferrite_blocks::Placement::Either) to one
    /// side of a cross-env split; required for such blocks in any doc
    /// that [`split_for_environment`](crate::env_split::split_for_environment)
    /// will carve. Omitted for hardware-pinned blocks (their
    /// `BlockSpec::placement` already decides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Environment>,
}

/// Wire — `src → dst`, each endpoint a `"block_id.port"` string.
///
/// Serialized as a JSON array `["src.out", "dst.in"]` for TS
/// compatibility. The struct form gives Rust callers named field access
/// and leaves room to grow (e.g. future metadata) without touching every
/// consumer.
///
/// The `dst` string can also carry a `"ui:<name>"` sentinel marking a
/// UI-terminal sink: the split pass inserts a `WsBridgeTx` on the
/// producing side and drops the wire on the consuming side. See
/// [`Wire::ui_sink_name`] and `env_split::split_for_environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wire {
    pub src: String,
    pub dst: String,
}

impl Wire {
    /// Build a wire from two endpoints.
    #[must_use]
    pub fn new(src: impl Into<String>, dst: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            dst: dst.into(),
        }
    }

    /// `Some(name)` if `dst` is a `"ui:<name>"` UI-terminal sink.
    #[must_use]
    pub fn ui_sink_name(&self) -> Option<&str> {
        self.dst.strip_prefix("ui:")
    }
}

impl Serialize for Wire {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(2))?;
        seq.serialize_element(&self.src)?;
        seq.serialize_element(&self.dst)?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Wire {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct WireVisitor;

        impl<'de> Visitor<'de> for WireVisitor {
            type Value = Wire;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON array [src, dst]")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Wire, A::Error> {
                let src: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let dst: String = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                if seq.next_element::<serde_json::Value>()?.is_some() {
                    return Err(A::Error::invalid_length(3, &self));
                }
                Ok(Wire { src, dst })
            }
        }

        d.deserialize_seq(WireVisitor)
    }
}

/// Top-level flowgraph document. Use [`FlowgraphDoc::from_json`] to load
/// from a slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowgraphDoc {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub environments: Vec<Environment>,
    /// Block id → declaration. `BTreeMap` keeps iteration deterministic
    /// (important for scheduler output ordering and for snapshot tests).
    pub blocks: BTreeMap<String, BlockInstanceDecl>,
    pub wires: Vec<Wire>,
}

impl FlowgraphDoc {
    /// Parse a flowgraph from raw JSON bytes.
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Serialise to pretty JSON — useful for round-trip tests + snapshot
    /// diffs. Production callers usually want `serde_json::to_vec`.
    pub fn to_json_pretty(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WBFM: &[u8] = include_bytes!("../../flowgraphs/wbfm.json");
    const WBAM: &[u8] = include_bytes!("../../flowgraphs/wbam.json");

    #[test]
    fn parses_shipped_wbfm_preset() {
        let doc = FlowgraphDoc::from_json(WBFM).expect("wbfm.json parses");
        assert_eq!(doc.name, "wbfm");
        // wbfm.json is authored cross-env: SoapySource + channelizer +
        // FFT tap on node, FM demod + AudioSink on browser. The FFT
        // tap terminates at `ui:fft`; env_split synthesizes the Tx on
        // load. `tee.out0 → decim.in` is the env crossing.
        assert_eq!(
            doc.environments,
            vec![Environment::Node, Environment::Browser]
        );
        assert_eq!(
            doc.blocks.keys().cloned().collect::<Vec<_>>(),
            vec!["audio", "chan", "decim", "demod", "fft", "logmag", "src", "tee"],
        );
        assert_eq!(doc.wires.len(), 8);
        // Spot-check one block's params survived the round-trip.
        let demod = doc.blocks.get("demod").expect("demod block present");
        assert_eq!(demod.type_name, "FmDemod");
        let params = demod.params.as_ref().expect("demod has params");
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(48_000.0));
        assert_eq!(params["max_deviation_hz"].as_f64(), Some(75_000.0));
        // Spot-check placements: chan pinned to node, audio inherits
        // browser from its WasmOnly spec (placement omitted in JSON).
        assert_eq!(
            doc.blocks.get("chan").unwrap().placement,
            Some(Environment::Node)
        );
        assert_eq!(doc.blocks.get("audio").unwrap().placement, None);
        // UI-terminal FFT wire is the sentinel form.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "logmag.out" && w.dst == "ui:fft"));
    }

    #[test]
    fn parses_shipped_wbam_preset() {
        let doc = FlowgraphDoc::from_json(WBAM).expect("wbam.json parses");
        assert_eq!(doc.name, "wbam");
        // Same cross-env shape as wbfm: Soapy source + FFT tap on node,
        // demod + audio on browser — the receivers pane toggle relies
        // on this symmetry for a clean demod-only reconfigure.
        assert_eq!(
            doc.environments,
            vec![Environment::Node, Environment::Browser]
        );
        assert_eq!(
            doc.blocks.keys().cloned().collect::<Vec<_>>(),
            vec!["audio", "chan", "decim", "demod", "fft", "logmag", "src", "tee"],
        );
        let demod = doc.blocks.get("demod").expect("demod block present");
        assert_eq!(demod.type_name, "AmDemod");
        let params = demod.params.as_ref().expect("demod has params");
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(48_000.0));
        assert_eq!(params["bias_tau_ms"].as_f64(), Some(100.0));
    }

    #[test]
    fn wbfm_and_wbam_have_identical_source_blocks() {
        // Receivers-pane toggle contract: swapping wbfm↔wbam must
        // leave `src` byte-identical so the runtime's diff plan
        // scopes the reconfigure to the browser half.
        let fm = FlowgraphDoc::from_json(WBFM).unwrap();
        let am = FlowgraphDoc::from_json(WBAM).unwrap();
        let fm_src = fm.blocks.get("src").expect("wbfm has src");
        let am_src = am.blocks.get("src").expect("wbam has src");
        assert_eq!(fm_src.type_name, am_src.type_name);
        assert_eq!(fm_src.params, am_src.params);
        assert_eq!(fm_src.placement, am_src.placement);
    }

    #[test]
    fn round_trips_through_serde() {
        let doc = FlowgraphDoc::from_json(WBFM).unwrap();
        let out = doc.to_json_pretty().unwrap();
        let doc2 = FlowgraphDoc::from_json(out.as_bytes()).unwrap();
        assert_eq!(doc.name, doc2.name);
        assert_eq!(doc.environments, doc2.environments);
        assert_eq!(doc.blocks.len(), doc2.blocks.len());
        assert_eq!(doc.wires, doc2.wires);
    }

    #[test]
    fn rejects_garbage() {
        assert!(FlowgraphDoc::from_json(b"not json at all").is_err());
        assert!(FlowgraphDoc::from_json(b"{}").is_err()); // missing required fields
    }

    #[test]
    fn wire_round_trips_as_two_element_array() {
        let w: Wire = serde_json::from_str(r#"["a.out", "b.in"]"#).unwrap();
        assert_eq!(w.src, "a.out");
        assert_eq!(w.dst, "b.in");
        let back = serde_json::to_string(&w).unwrap();
        assert_eq!(back, r#"["a.out","b.in"]"#);
    }

    #[test]
    fn wire_ui_sink_name_strips_prefix() {
        let w = Wire::new("src.out", "ui:waterfall");
        assert_eq!(w.ui_sink_name(), Some("waterfall"));
        assert_eq!(Wire::new("src.out", "sink.in").ui_sink_name(), None);
    }

    #[test]
    fn wire_rejects_three_element_array() {
        let err = serde_json::from_str::<Wire>(r#"["a.out", "b.in", "extra"]"#);
        assert!(err.is_err());
    }

    #[test]
    fn environment_json_values_match_ts() {
        // Must serialise as the lowercase strings `"browser"` / `"node"`
        // so the Rust side round-trips files authored against the TS
        // types. Any drift here breaks cross-env preset loading.
        let browser = serde_json::to_string(&Environment::Browser).unwrap();
        let node = serde_json::to_string(&Environment::Node).unwrap();
        assert_eq!(browser, "\"browser\"");
        assert_eq!(node, "\"node\"");
    }
}
