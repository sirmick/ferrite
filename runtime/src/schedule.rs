//! Topological scheduling + wire plan.
//!
//! Consumes a [`ValidatedDoc`] (the registry-independent graph) and emits
//! two artefacts the tick pump (a later commit) will drive off:
//!
//!   * [`topological_order`] — block ids ordered so every producer
//!     appears before every consumer. Ties are broken lexicographically
//!     on instance id so tests can pin exact orderings.
//!   * [`WirePlan`] — reverse-index of wires by consumer: for each block,
//!     a map from its input port name to the producing `(block, port)`.
//!
//! Both functions assume validation passed — dangling endpoints or
//! cycles would be caller bugs and produce [`anyhow::Error`]s rather
//! than [`ValidationError`]s.
//!
//! The TS counterpart carries `portType` on each `InputSource` so the
//! tick pump can allocate the right buffer. We can't do that here until
//! `BlockSpec` is reachable from the runtime — the registry-dependent
//! commit that adds it will extend [`InputSource`].

use std::collections::BTreeMap;

use ferrite_blocks::PortType;

use crate::doc::FlowgraphDoc;
use crate::validate::{split_endpoint, ValidatedDoc};

/// Where one consumer input port's samples come from. `port_type` is
/// populated by [`instantiate_flowgraph`](crate::instantiate::instantiate_flowgraph);
/// callers that build a [`Schedule`] directly (without registry lookup)
/// leave it as `None` — that path is for tests and graph-shape
/// inspection only, not for live execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSource {
    pub source_block: String,
    pub source_port: String,
    pub port_type: Option<PortType>,
}

/// Per block id → (input port name → producer). Every block in the doc
/// is present in the outer map, with an empty inner map when the block
/// has no inputs — callers iterate uniformly.
pub type WirePlan = BTreeMap<String, BTreeMap<String, InputSource>>;

/// Scheduler artefacts. Cheap to build, immutable after construction.
/// Rebuild when the graph changes.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub order: Vec<String>,
    pub wire_plan: WirePlan,
}

impl Schedule {
    pub fn from_validated(v: &ValidatedDoc<'_>) -> anyhow::Result<Self> {
        let order = topological_order(v.doc)?;
        let wire_plan = build_wire_plan(v.doc);
        Ok(Self { order, wire_plan })
    }
}

/// Returns block ids in a run order: every producer before every
/// consumer; ready-set ties broken lexicographically.
pub fn topological_order(doc: &FlowgraphDoc) -> anyhow::Result<Vec<String>> {
    let ids: Vec<String> = doc.blocks.keys().cloned().collect();
    let mut indegree: BTreeMap<String, usize> = ids.iter().map(|k| (k.clone(), 0)).collect();
    let mut out: BTreeMap<String, Vec<String>> =
        ids.iter().map(|k| (k.clone(), Vec::new())).collect();

    for wire in &doc.wires {
        let from = split_endpoint(&wire[0]).0.to_string();
        let to = split_endpoint(&wire[1]).0.to_string();
        if from == to {
            // Self-loops are cycles; caller should have rejected. Guard anyway.
            continue;
        }
        let outs = out.get_mut(&from).ok_or_else(|| {
            anyhow::anyhow!("scheduler: wire references unknown source block {from}")
        })?;
        if !outs.iter().any(|x| x == &to) {
            outs.push(to.clone());
            *indegree.get_mut(&to).ok_or_else(|| {
                anyhow::anyhow!("scheduler: wire references unknown sink block {to}")
            })? += 1;
        }
    }

    // Ready set — keep it sorted so we emit ties in lexicographic order.
    let mut ready: Vec<String> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    ready.sort();

    let mut order: Vec<String> = Vec::with_capacity(ids.len());
    while let Some(next) = (!ready.is_empty()).then(|| ready.remove(0)) {
        order.push(next.clone());
        let mut newly_ready: Vec<String> = Vec::new();
        if let Some(outs) = out.get(&next) {
            for nb in outs {
                let d = indegree.get_mut(nb).unwrap();
                *d -= 1;
                if *d == 0 {
                    newly_ready.push(nb.clone());
                }
            }
        }
        for nb in newly_ready {
            // Binary-insert into the ready list to preserve order.
            let pos = ready.binary_search(&nb).unwrap_or_else(|e| e);
            ready.insert(pos, nb);
        }
    }

    if order.len() != ids.len() {
        anyhow::bail!(
            "scheduler bug: topological order missing blocks — graph contains a cycle \
             but was not rejected by validate_doc"
        );
    }
    Ok(order)
}

/// Builds the per-block reverse-index of wires.
pub fn build_wire_plan(doc: &FlowgraphDoc) -> WirePlan {
    let mut plan: WirePlan = doc
        .blocks
        .keys()
        .map(|k| (k.clone(), BTreeMap::new()))
        .collect();
    for wire in &doc.wires {
        let (src_block, src_port) = split_endpoint(&wire[0]);
        let (dst_block, dst_port) = split_endpoint(&wire[1]);
        plan.get_mut(dst_block).expect("dst block present").insert(
            dst_port.to_string(),
            InputSource {
                source_block: src_block.to_string(),
                source_port: src_port.to_string(),
                port_type: None,
            },
        );
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_doc;

    const WBFM: &[u8] = include_bytes!("../../flowgraphs/wbfm.json");

    #[test]
    fn schedules_wbfm_in_expected_order() {
        let doc = FlowgraphDoc::from_json(WBFM).unwrap();
        let v = validate_doc(&doc).unwrap();
        let s = Schedule::from_validated(&v).unwrap();
        // wbfm.json: src → chan → decim → demod → audio. The block
        // map is alphabetically sorted, but the scheduler must still
        // emit source-before-sink because of the data-flow constraints.
        assert_eq!(s.order, vec!["src", "chan", "decim", "demod", "audio"]);
    }

    #[test]
    fn wire_plan_reverse_indexes_producers() {
        let doc = FlowgraphDoc::from_json(WBFM).unwrap();
        let v = validate_doc(&doc).unwrap();
        let s = Schedule::from_validated(&v).unwrap();
        let demod = &s.wire_plan["demod"];
        assert_eq!(demod["in"].source_block, "decim");
        assert_eq!(demod["in"].source_port, "out");
        // src has no inputs — still present with an empty map.
        assert!(s.wire_plan["src"].is_empty());
    }

    #[test]
    fn ready_set_ties_break_lexicographically() {
        // Two sources, no dependencies between them. Expected order:
        // sorted by id (a, b), then consumers in dependency order.
        let doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name": "t",
                "environments": ["browser"],
                "blocks": {
                    "b": {"type": "Src"},
                    "a": {"type": "Src"},
                    "mix": {"type": "Mix"}
                },
                "wires": [
                    ["a.out", "mix.a"],
                    ["b.out", "mix.b"]
                ]
            }"#,
        )
        .unwrap();
        let v = validate_doc(&doc).unwrap();
        let s = Schedule::from_validated(&v).unwrap();
        assert_eq!(s.order, vec!["a", "b", "mix"]);
    }
}
