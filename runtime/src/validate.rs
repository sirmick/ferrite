//! Registry-independent graph validation.
//!
//! The TS counterpart splits validation across `validate.ts` (this module)
//! and `instantiate.ts` (the registry-dependent phases). This file owns
//! the shape/fan/dag/connectivity/wire_endpoints passes — everything that
//! can be checked with the JSON doc alone, no `BlockSpec` lookups.
//!
//! Phases producing `ValidationError` here:
//!
//!   * `shape`           — JSON matched the `FlowgraphDoc` schema (but we
//!                         re-assert non-emptiness and endpoint syntax
//!                         that `serde` alone won't catch)
//!   * `fan`             — every port has at most one wire
//!   * `dag`             — the graph has no cycles
//!   * `connectivity`    — warn (non-fatal) on isolated blocks
//!   * `wire_endpoints`  — wire endpoints reference blocks that exist
//!                         (port existence + type match remain for the
//!                         registry-dependent pass in a later commit)
//!
//! Errors accumulate inside a phase so a flowgraph author sees every
//! problem at once, but we bail before the DAG pass if earlier phases
//! produced anything — cycle detection on malformed input is noise.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

use crate::doc::FlowgraphDoc;

/// The validation phase a [`ValidationError`] came from. Matches the TS
/// `ValidationError.phase` enum. Registry-dependent phases
/// (`env_match`, `params`, `wire_type_match`, `rate_negotiation`) are
/// reserved for the instantiator commit that introduces the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Shape,
    EnvMatch,
    Params,
    WireEndpoints,
    WireTypeMatch,
    Fan,
    Dag,
    Connectivity,
    RateNegotiation,
}

/// One thing that's wrong with (or suspicious about) a flowgraph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub phase: Phase,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<crate::doc::Wire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

/// Raised when any fatal validation phase produces an error. Connectivity
/// warnings never reach here — they're returned from
/// [`validate_doc`] alongside the OK path.
#[derive(Debug, Error)]
#[error("flowgraph validation failed: {summary}")]
pub struct FlowgraphValidationError {
    pub errors: Vec<ValidationError>,
    summary: String,
}

impl FlowgraphValidationError {
    pub(crate) fn new(errors: Vec<ValidationError>) -> Self {
        let summary = errors
            .iter()
            .map(|e| format!("[{:?}] {}", e.phase, e.error))
            .collect::<Vec<_>>()
            .join("; ");
        Self { errors, summary }
    }
}

/// Outcome of a successful (registry-independent) validation pass. The
/// `warnings` list carries non-fatal `connectivity` notes — graph authors
/// may want to surface those in tools without failing the build.
#[derive(Debug, Clone)]
pub struct ValidatedDoc<'a> {
    pub doc: &'a FlowgraphDoc,
    pub warnings: Vec<ValidationError>,
}

/// Validate a `FlowgraphDoc` through every registry-independent phase.
/// Fails fast across phases: if shape errors are present we don't bother
/// checking DAG, since cycle detection on malformed data just confuses.
pub fn validate_doc(doc: &FlowgraphDoc) -> Result<ValidatedDoc<'_>, FlowgraphValidationError> {
    let mut errors = Vec::new();
    errors.extend(check_shape(doc));
    errors.extend(check_wire_endpoints(doc));
    if !errors.is_empty() {
        return Err(FlowgraphValidationError::new(errors));
    }
    errors.extend(check_fan(doc));
    errors.extend(check_dag(doc));
    if !errors.is_empty() {
        return Err(FlowgraphValidationError::new(errors));
    }
    let warnings = check_connectivity(doc);
    Ok(ValidatedDoc { doc, warnings })
}

// ---------------------------------------------------------------------------
// Shape — post-serde tightening.
// ---------------------------------------------------------------------------

fn check_shape(doc: &FlowgraphDoc) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if doc.name.is_empty() {
        errors.push(ValidationError {
            phase: Phase::Shape,
            error: "`name` must be a non-empty string".into(),
            wire: None,
            block: None,
            port: None,
        });
    }
    if doc.environments.is_empty() {
        errors.push(ValidationError {
            phase: Phase::Shape,
            error: "`environments` must be non-empty".into(),
            wire: None,
            block: None,
            port: None,
        });
    }
    for (id, _decl) in &doc.blocks {
        if id.is_empty() {
            errors.push(ValidationError {
                phase: Phase::Shape,
                error: "block id must be non-empty".into(),
                wire: None,
                block: Some(id.clone()),
                port: None,
            });
        }
    }
    for (i, wire) in doc.wires.iter().enumerate() {
        for endpoint in wire {
            if !endpoint.contains('.') {
                errors.push(ValidationError {
                    phase: Phase::Shape,
                    error: format!(
                        "wire[{i}] endpoints must be \"instance.port\" strings (got {endpoint:?})"
                    ),
                    wire: Some(wire.clone()),
                    block: None,
                    port: None,
                });
            }
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Wire endpoints (block existence only — port existence is registry-side).
// ---------------------------------------------------------------------------

fn check_wire_endpoints(doc: &FlowgraphDoc) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for wire in &doc.wires {
        let (from_block, _) = split_endpoint(&wire[0]);
        let (to_block, _) = split_endpoint(&wire[1]);
        if !doc.blocks.contains_key(from_block) {
            errors.push(ValidationError {
                phase: Phase::WireEndpoints,
                error: format!("wire source references unknown block {from_block:?}"),
                wire: Some(wire.clone()),
                block: Some(from_block.to_string()),
                port: None,
            });
        }
        if !doc.blocks.contains_key(to_block) {
            errors.push(ValidationError {
                phase: Phase::WireEndpoints,
                error: format!("wire sink references unknown block {to_block:?}"),
                wire: Some(wire.clone()),
                block: Some(to_block.to_string()),
                port: None,
            });
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Fan — at most one wire per output, at most one wire per input.
// ---------------------------------------------------------------------------

fn check_fan(doc: &FlowgraphDoc) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut seen_out: HashSet<&str> = HashSet::new();
    let mut seen_in: HashSet<&str> = HashSet::new();
    for wire in &doc.wires {
        if !seen_out.insert(&wire[0]) {
            errors.push(ValidationError {
                phase: Phase::Fan,
                error: format!(
                    "output port {:?} is wired more than once — use a Tee block to broadcast",
                    wire[0]
                ),
                wire: Some(wire.clone()),
                block: None,
                port: None,
            });
        }
        if !seen_in.insert(&wire[1]) {
            errors.push(ValidationError {
                phase: Phase::Fan,
                error: format!("input port {:?} is wired more than once", wire[1]),
                wire: Some(wire.clone()),
                block: None,
                port: None,
            });
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// DAG — white/grey/black DFS.
// ---------------------------------------------------------------------------

fn check_dag(doc: &FlowgraphDoc) -> Vec<ValidationError> {
    let mut adj: HashMap<&str, HashSet<&str>> = HashMap::new();
    for id in doc.blocks.keys() {
        adj.insert(id.as_str(), HashSet::new());
    }
    for wire in &doc.wires {
        let (from, _) = split_endpoint(&wire[0]);
        let (to, _) = split_endpoint(&wire[1]);
        if adj.contains_key(from) && adj.contains_key(to) {
            adj.get_mut(from).unwrap().insert(to);
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }
    let mut color: HashMap<&str, Color> = doc
        .blocks
        .keys()
        .map(|k| (k.as_str(), Color::White))
        .collect();

    for root in doc.blocks.keys() {
        let root = root.as_str();
        if color[root] != Color::White {
            continue;
        }
        // Iterative DFS — the stack pairs each node with an iterator
        // over its unvisited neighbours.
        let neighbours: Vec<&str> = adj[root].iter().copied().collect();
        let mut stack: Vec<(&str, std::vec::IntoIter<&str>)> = vec![(root, neighbours.into_iter())];
        color.insert(root, Color::Grey);
        while let Some((id, iter)) = stack.last_mut() {
            if let Some(nb) = iter.next() {
                match color[nb] {
                    Color::Grey => {
                        return vec![ValidationError {
                            phase: Phase::Dag,
                            error: format!("cycle detected: {id} → {nb}"),
                            wire: None,
                            block: None,
                            port: None,
                        }];
                    }
                    Color::White => {
                        color.insert(nb, Color::Grey);
                        let next_nb: Vec<&str> = adj[nb].iter().copied().collect();
                        stack.push((nb, next_nb.into_iter()));
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(id, Color::Black);
                stack.pop();
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Connectivity — warn on isolated blocks.
// ---------------------------------------------------------------------------

fn check_connectivity(doc: &FlowgraphDoc) -> Vec<ValidationError> {
    let mut touched: HashSet<&str> = HashSet::new();
    for wire in &doc.wires {
        touched.insert(split_endpoint(&wire[0]).0);
        touched.insert(split_endpoint(&wire[1]).0);
    }
    let mut warnings = Vec::new();
    for id in doc.blocks.keys() {
        if !touched.contains(id.as_str()) {
            warnings.push(ValidationError {
                phase: Phase::Connectivity,
                error: format!("block {id:?} has no wires (isolated)"),
                wire: None,
                block: Some(id.clone()),
                port: None,
            });
        }
    }
    warnings
}

// ---------------------------------------------------------------------------
// Helper.
// ---------------------------------------------------------------------------

pub(crate) fn split_endpoint(endpoint: &str) -> (&str, &str) {
    match endpoint.find('.') {
        Some(dot) => (&endpoint[..dot], &endpoint[dot + 1..]),
        None => (endpoint, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WBFM: &[u8] = include_bytes!("../../flowgraphs/wbfm.json");

    #[test]
    fn accepts_shipped_wbfm() {
        let doc = FlowgraphDoc::from_json(WBFM).unwrap();
        let v = validate_doc(&doc).expect("wbfm passes registry-independent validation");
        assert!(
            v.warnings.is_empty(),
            "unexpected warnings: {:?}",
            v.warnings
        );
    }

    fn minimal_doc() -> FlowgraphDoc {
        serde_json::from_str(
            r#"{
                "name": "t",
                "environments": ["browser"],
                "blocks": {
                    "a": {"type": "X"},
                    "b": {"type": "Y"}
                },
                "wires": [["a.out", "b.in"]]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_empty_name() {
        let mut d = minimal_doc();
        d.name.clear();
        let err = validate_doc(&d).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::Shape));
    }

    #[test]
    fn rejects_wire_to_unknown_block() {
        let mut d = minimal_doc();
        d.wires.push(["a.out2".into(), "nope.in".into()]);
        let err = validate_doc(&d).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::WireEndpoints));
    }

    #[test]
    fn rejects_fan_in_on_same_port() {
        let mut d = minimal_doc();
        d.blocks.insert(
            "c".into(),
            crate::doc::BlockInstanceDecl {
                type_name: "Z".into(),
                params: None,
                placement: None,
            },
        );
        d.wires.push(["c.out".into(), "b.in".into()]);
        let err = validate_doc(&d).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::Fan));
    }

    #[test]
    fn rejects_cycle() {
        let mut d = minimal_doc();
        d.wires.push(["b.out".into(), "a.in".into()]);
        let err = validate_doc(&d).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::Dag));
    }

    #[test]
    fn warns_on_isolated_block() {
        let mut d = minimal_doc();
        d.blocks.insert(
            "orphan".into(),
            crate::doc::BlockInstanceDecl {
                type_name: "Z".into(),
                params: None,
                placement: None,
            },
        );
        let v = validate_doc(&d).unwrap();
        assert_eq!(v.warnings.len(), 1);
        assert_eq!(v.warnings[0].phase, Phase::Connectivity);
        assert_eq!(v.warnings[0].block.as_deref(), Some("orphan"));
    }

    #[test]
    fn rejects_endpoint_without_port() {
        let mut d = minimal_doc();
        d.wires[0][0] = "a".into(); // missing ".port"
        let err = validate_doc(&d).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::Shape));
    }
}
