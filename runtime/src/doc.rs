//! Flowgraph JSON document — serde shape.
//!
//! Mirrors `FlowgraphDoc` in `packages/flowgraph-runtime/src/types.ts`
//! byte-for-byte so the same JSON file parses on either side. Field
//! names stay camelCase-free (the JSON already uses snake-ish lowercase
//! keys like `environments`, `blocks`, `wires`) — serde's default
//! matches the TS shape without any `#[serde(rename)]` on this struct.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
}

/// Wire endpoints are `"instance_id.port_name"` strings. Encoded as a
/// two-element JSON array to match the TS shape (`readonly [string, string]`).
pub type Wire = [String; 2];

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

    #[test]
    fn parses_shipped_wbfm_preset() {
        let doc = FlowgraphDoc::from_json(WBFM).expect("wbfm.json parses");
        assert_eq!(doc.name, "wbfm");
        assert_eq!(doc.environments, vec![Environment::Browser]);
        assert_eq!(
            doc.blocks.keys().cloned().collect::<Vec<_>>(),
            vec!["audio", "decim", "demod", "src"],
        );
        assert_eq!(doc.wires.len(), 3);
        // Spot-check one block's params survived the round-trip.
        let demod = doc.blocks.get("demod").expect("demod block present");
        assert_eq!(demod.type_name, "FmDemod");
        let params = demod.params.as_ref().expect("demod has params");
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(48_000.0));
        assert_eq!(params["max_deviation_hz"].as_f64(), Some(75_000.0));
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
