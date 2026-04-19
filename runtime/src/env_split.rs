//! Environment split — carves a cross-env preset into the single-env
//! subgraph that one runtime instance will actually run.
//!
//! A preset is authored as one doc. Blocks are placed in either the
//! native ("node") half or the browser half via `placement` on each
//! instance; blocks whose `BlockSpec::placement` is `NativeOnly` or
//! `WasmOnly` inherit that side automatically. Wires that cross the
//! boundary are replaced at split time with a `WsBridgeTx`/`WsBridgeRx`
//! pair: the producing side keeps its block and gets a new Tx
//! consuming the signal for transport; the consuming side gets a new
//! Rx producing the signal and keeps its own consumer.
//!
//! Stream IDs are allocated deterministically from the doc's wire list
//! (in input order, counting every node→browser crossing) so both halves
//! of the split agree on which Tx pairs with which Rx without any
//! out-of-band channel.
//!
//! Today only `node → browser` crossings are supported. The receive-
//! side block on native (`WsBridgeTx`'s mirror) does not exist yet;
//! browser-to-server dataflow is an M3+ concern when reconfigure events
//! and control channels land.

use std::collections::BTreeMap;

use ferrite_blocks::Placement;
use serde_json::json;
use thiserror::Error;

use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};
use crate::instantiate::SpecRegistry;
use crate::validate::split_endpoint;

/// Base `stream_id` for auto-inserted cross-env bridges. Chosen clear of
/// the VFO range (`VFO_STREAM_BASE = 2` in `docs/02-protocol.md`,
/// allocated up as VFOs are added), so a preset with a handful of
/// crossings never collides with live VFO streams.
pub const CROSS_ENV_STREAM_BASE: u32 = 1000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SplitError {
    #[error("block {id:?} declares unknown type {type_name:?}")]
    UnknownType { id: String, type_name: String },
    #[error(
        "block {id:?} has explicit placement {explicit:?} but its type {type_name:?} requires {required:?}"
    )]
    PlacementConflict {
        id: String,
        type_name: String,
        explicit: Environment,
        required: Environment,
    },
    #[error(
        "block {id:?} of type {type_name:?} is Placement::Either — flowgraph author must pin it with an explicit `placement` field (either `node` or `browser`)"
    )]
    PlacementUnresolved { id: String, type_name: String },
    #[error(
        "wire {wire:?} crosses from browser to node; only node→browser crossings are supported today"
    )]
    UnsupportedCrossing { wire: Wire },
    #[error("split would synthesize bridge block {id:?} but the source doc already uses that id")]
    BridgeNameCollision { id: String },
}

/// Split `doc` for `env`: keep only blocks placed in `env`, keep only
/// wires internal to `env`, and terminate each cross-env wire with a
/// synthesized `WsBridgeTx` (on the node half) or `WsBridgeRx` (on the
/// browser half). The two halves produced by calling this with both
/// environments share stream IDs so their bridge pairs line up.
pub fn split_for_environment(
    doc: &FlowgraphDoc,
    env: Environment,
    registry: &dyn SpecRegistry,
) -> Result<FlowgraphDoc, SplitError> {
    let placements = resolve_placements(doc, registry)?;

    // Reject unsupported browser→node crossings up-front so the split
    // loop below can assume every crossing goes the other direction.
    for wire in &doc.wires {
        let (src, _) = split_endpoint(&wire[0]);
        let (dst, _) = split_endpoint(&wire[1]);
        if placements[src] == Environment::Browser && placements[dst] == Environment::Node {
            return Err(SplitError::UnsupportedCrossing { wire: wire.clone() });
        }
    }

    let mut new_blocks: BTreeMap<String, BlockInstanceDecl> = doc
        .blocks
        .iter()
        .filter(|(id, _)| placements[*id] == env)
        .map(|(id, decl)| {
            let mut d = decl.clone();
            // Pin so the output doc carries placement on every block —
            // downstream tools don't have to consult the registry again.
            d.placement = Some(env);
            (id.clone(), d)
        })
        .collect();

    let mut new_wires: Vec<Wire> = Vec::new();
    let mut crossing_index: u32 = 0;

    for wire in &doc.wires {
        let (src_block, _) = split_endpoint(&wire[0]);
        let (dst_block, _) = split_endpoint(&wire[1]);
        let src_env = placements[src_block];
        let dst_env = placements[dst_block];

        match (src_env == env, dst_env == env) {
            (true, true) => new_wires.push(wire.clone()),
            (true, false) => {
                // This half produces, the other half consumes. Add a
                // WsBridgeTx to terminate the node-side wire.
                let sid = CROSS_ENV_STREAM_BASE + crossing_index;
                let bridge_id = format!("__bridge_tx_{sid}");
                insert_bridge(
                    &mut new_blocks,
                    doc,
                    bridge_id.clone(),
                    "WsBridgeTx",
                    env,
                    sid,
                )?;
                new_wires.push([wire[0].clone(), format!("{bridge_id}.in")]);
                crossing_index += 1;
            }
            (false, true) => {
                // The other half produces, this half consumes. Add a
                // WsBridgeRx to source the browser-side wire.
                let sid = CROSS_ENV_STREAM_BASE + crossing_index;
                let bridge_id = format!("__bridge_rx_{sid}");
                insert_bridge(
                    &mut new_blocks,
                    doc,
                    bridge_id.clone(),
                    "WsBridgeRx",
                    env,
                    sid,
                )?;
                new_wires.push([format!("{bridge_id}.out"), wire[1].clone()]);
                crossing_index += 1;
            }
            (false, false) => {
                // Wire internal to the other half — drop, no slot consumed.
            }
        }
    }

    Ok(FlowgraphDoc {
        schema: doc.schema.clone(),
        name: doc.name.clone(),
        label: doc.label.clone(),
        description: doc.description.clone(),
        environments: vec![env],
        blocks: new_blocks,
        wires: new_wires,
    })
}

fn insert_bridge(
    new_blocks: &mut BTreeMap<String, BlockInstanceDecl>,
    source_doc: &FlowgraphDoc,
    id: String,
    type_name: &'static str,
    env: Environment,
    stream_id: u32,
) -> Result<(), SplitError> {
    if new_blocks.contains_key(&id) || source_doc.blocks.contains_key(&id) {
        return Err(SplitError::BridgeNameCollision { id });
    }
    new_blocks.insert(
        id,
        BlockInstanceDecl {
            type_name: type_name.to_string(),
            params: Some(json!({ "stream_id": stream_id })),
            placement: Some(env),
        },
    );
    Ok(())
}

fn resolve_placements(
    doc: &FlowgraphDoc,
    registry: &dyn SpecRegistry,
) -> Result<BTreeMap<String, Environment>, SplitError> {
    let mut out = BTreeMap::new();
    for (id, decl) in &doc.blocks {
        let spec = registry
            .get(&decl.type_name)
            .ok_or_else(|| SplitError::UnknownType {
                id: id.clone(),
                type_name: decl.type_name.clone(),
            })?;
        let resolved = match (decl.placement, spec.placement) {
            (Some(e), Placement::Either) => e,
            (Some(Environment::Node) | None, Placement::NativeOnly) => Environment::Node,
            (Some(Environment::Browser) | None, Placement::WasmOnly) => Environment::Browser,
            (None, Placement::Either) => {
                return Err(SplitError::PlacementUnresolved {
                    id: id.clone(),
                    type_name: decl.type_name.clone(),
                })
            }
            (Some(explicit), block_ph) => {
                let required = match block_ph {
                    Placement::NativeOnly => Environment::Node,
                    Placement::WasmOnly => Environment::Browser,
                    Placement::Either => unreachable!("handled by first match arm"),
                };
                return Err(SplitError::PlacementConflict {
                    id: id.clone(),
                    type_name: decl.type_name.clone(),
                    explicit,
                    required,
                });
            }
        };
        out.insert(id.clone(), resolved);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_blocks::{BlockSpec, ParamSpec, PortSpec, PortType};

    struct StubRegistry(Vec<(&'static str, &'static BlockSpec)>);

    impl SpecRegistry for StubRegistry {
        fn get(&self, type_name: &str) -> Option<BlockSpec> {
            self.0
                .iter()
                .find(|(n, _)| *n == type_name)
                .map(|(_, s)| **s)
        }
    }

    const IQ_OUT: &[PortSpec] = &[PortSpec {
        name: "out",
        port_type: PortType::IqF32,
    }];
    const IQ_IN: &[PortSpec] = &[PortSpec {
        name: "in",
        port_type: PortType::IqF32,
    }];
    const NO_PORTS: &[PortSpec] = &[];
    const NO_PARAMS: &[ParamSpec] = &[];

    const HW_SRC: BlockSpec = BlockSpec {
        type_name: "HwSrc",
        placement: Placement::NativeOnly,
        inputs: NO_PORTS,
        outputs: IQ_OUT,
        params: NO_PARAMS,
    };
    const WASM_SINK: BlockSpec = BlockSpec {
        type_name: "WasmSink",
        placement: Placement::WasmOnly,
        inputs: IQ_IN,
        outputs: NO_PORTS,
        params: NO_PARAMS,
    };
    const EITHER: BlockSpec = BlockSpec {
        type_name: "Either",
        placement: Placement::Either,
        inputs: IQ_IN,
        outputs: IQ_OUT,
        params: NO_PARAMS,
    };
    const WS_TX: BlockSpec = BlockSpec {
        type_name: "WsBridgeTx",
        placement: Placement::NativeOnly,
        inputs: IQ_IN,
        outputs: NO_PORTS,
        params: NO_PARAMS,
    };
    const WS_RX: BlockSpec = BlockSpec {
        type_name: "WsBridgeRx",
        placement: Placement::WasmOnly,
        inputs: NO_PORTS,
        outputs: IQ_OUT,
        params: NO_PARAMS,
    };

    fn stub() -> StubRegistry {
        StubRegistry(vec![
            ("HwSrc", &HW_SRC),
            ("WasmSink", &WASM_SINK),
            ("Either", &EITHER),
            ("WsBridgeTx", &WS_TX),
            ("WsBridgeRx", &WS_RX),
        ])
    }

    fn doc_from(json: &str) -> FlowgraphDoc {
        serde_json::from_str(json).unwrap()
    }

    /// Cross-env reference doc: `HwSrc` (node) → mid (Either, node) →
    /// [boundary] → sink (`WasmSink`, browser). One crossing.
    fn cross_env_doc() -> FlowgraphDoc {
        doc_from(
            r#"{
                "name": "demo",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  {"type": "HwSrc"},
                    "mid":  {"type": "Either", "placement": "node"},
                    "sink": {"type": "WasmSink"}
                },
                "wires": [
                    ["src.out", "mid.in"],
                    ["mid.out", "sink.in"]
                ]
            }"#,
        )
    }

    #[test]
    fn split_node_half_keeps_node_blocks_and_adds_tx_bridge() {
        let doc = cross_env_doc();
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();

        assert_eq!(node.environments, vec![Environment::Node]);
        assert!(node.blocks.contains_key("src"));
        assert!(node.blocks.contains_key("mid"));
        assert!(!node.blocks.contains_key("sink"));

        let bridge_id = format!("__bridge_tx_{CROSS_ENV_STREAM_BASE}");
        let bridge = node
            .blocks
            .get(&bridge_id)
            .expect("tx bridge inserted at crossing");
        assert_eq!(bridge.type_name, "WsBridgeTx");
        assert_eq!(bridge.placement, Some(Environment::Node));
        assert_eq!(
            bridge.params.as_ref().unwrap()["stream_id"].as_u64(),
            Some(u64::from(CROSS_ENV_STREAM_BASE)),
        );

        // Internal wire kept; crossing wire rewritten to end at bridge.
        assert!(node
            .wires
            .iter()
            .any(|w| w[0] == "src.out" && w[1] == "mid.in"));
        assert!(node
            .wires
            .iter()
            .any(|w| w[0] == "mid.out" && w[1] == format!("{bridge_id}.in")));
    }

    #[test]
    fn split_browser_half_keeps_sink_and_adds_rx_bridge_with_matching_sid() {
        let doc = cross_env_doc();
        let browser = split_for_environment(&doc, Environment::Browser, &stub()).unwrap();

        assert_eq!(browser.environments, vec![Environment::Browser]);
        assert!(browser.blocks.contains_key("sink"));
        assert!(!browser.blocks.contains_key("src"));
        assert!(!browser.blocks.contains_key("mid"));

        let bridge_id = format!("__bridge_rx_{CROSS_ENV_STREAM_BASE}");
        let bridge = browser
            .blocks
            .get(&bridge_id)
            .expect("rx bridge inserted at crossing");
        assert_eq!(bridge.type_name, "WsBridgeRx");
        assert_eq!(bridge.placement, Some(Environment::Browser));
        // Same stream_id as the Tx half — this is the whole point.
        assert_eq!(
            bridge.params.as_ref().unwrap()["stream_id"].as_u64(),
            Some(u64::from(CROSS_ENV_STREAM_BASE)),
        );
        assert!(browser
            .wires
            .iter()
            .any(|w| w[0] == format!("{bridge_id}.out") && w[1] == "sink.in"));
    }

    #[test]
    fn hardware_pinned_blocks_inherit_placement_from_spec() {
        // No explicit `placement` on src/sink: one's NativeOnly, the
        // other's WasmOnly, so the split still knows what to do.
        let doc = doc_from(
            r#"{
                "name": "inherit",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  {"type": "HwSrc"},
                    "sink": {"type": "WasmSink"}
                },
                "wires": [
                    ["src.out", "sink.in"]
                ]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        assert!(node.blocks.contains_key("src"));
        let bridge_id = format!("__bridge_tx_{CROSS_ENV_STREAM_BASE}");
        assert!(node.blocks.contains_key(&bridge_id));
    }

    #[test]
    fn either_without_explicit_placement_is_rejected() {
        let doc = doc_from(
            r#"{
                "name": "ambig",
                "environments": ["node", "browser"],
                "blocks": {
                    "mid": {"type": "Either"}
                },
                "wires": []
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::PlacementUnresolved { .. }));
    }

    #[test]
    fn explicit_placement_conflicting_with_block_spec_is_rejected() {
        let doc = doc_from(
            r#"{
                "name": "conflict",
                "environments": ["node", "browser"],
                "blocks": {
                    "src": {"type": "HwSrc", "placement": "browser"}
                },
                "wires": []
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::PlacementConflict { .. }));
    }

    #[test]
    fn browser_to_node_wire_is_unsupported() {
        let doc = doc_from(
            r#"{
                "name": "reverse",
                "environments": ["node", "browser"],
                "blocks": {
                    "b": {"type": "Either", "placement": "browser"},
                    "n": {"type": "Either", "placement": "node"}
                },
                "wires": [
                    ["b.out", "n.in"]
                ]
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::UnsupportedCrossing { .. }));
    }

    #[test]
    fn multiple_crossings_get_consecutive_stream_ids() {
        let doc = doc_from(
            r#"{
                "name": "two-cross",
                "environments": ["node", "browser"],
                "blocks": {
                    "src1":  {"type": "HwSrc"},
                    "src2":  {"type": "HwSrc"},
                    "sink1": {"type": "WasmSink"},
                    "sink2": {"type": "WasmSink"}
                },
                "wires": [
                    ["src1.out", "sink1.in"],
                    ["src2.out", "sink2.in"]
                ]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        let tx0 = format!("__bridge_tx_{CROSS_ENV_STREAM_BASE}");
        let tx1 = format!("__bridge_tx_{}", CROSS_ENV_STREAM_BASE + 1);
        assert!(node.blocks.contains_key(&tx0));
        assert!(node.blocks.contains_key(&tx1));

        let browser = split_for_environment(&doc, Environment::Browser, &stub()).unwrap();
        let rx0 = format!("__bridge_rx_{CROSS_ENV_STREAM_BASE}");
        let rx1 = format!("__bridge_rx_{}", CROSS_ENV_STREAM_BASE + 1);
        assert!(browser.blocks.contains_key(&rx0));
        assert!(browser.blocks.contains_key(&rx1));
    }

    #[test]
    fn split_output_is_round_trippable_through_serde() {
        let doc = cross_env_doc();
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        let json = node.to_json_pretty().unwrap();
        let reparsed = FlowgraphDoc::from_json(json.as_bytes()).unwrap();
        assert_eq!(reparsed.blocks.len(), node.blocks.len());
        assert_eq!(reparsed.wires, node.wires);
        assert_eq!(reparsed.environments, vec![Environment::Node]);
    }

    #[test]
    fn single_env_doc_is_returned_unchanged_shape() {
        // Graph entirely inside node — no bridges, no dropped blocks.
        let doc = doc_from(
            r#"{
                "name": "all-node",
                "environments": ["node"],
                "blocks": {
                    "a": {"type": "Either", "placement": "node"},
                    "b": {"type": "Either", "placement": "node"}
                },
                "wires": [["a.out", "b.in"]]
            }"#,
        );
        let out = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        assert_eq!(out.blocks.len(), 2);
        assert_eq!(out.wires.len(), 1);
        // Placement is now pinned on every surviving block.
        assert_eq!(out.blocks["a"].placement, Some(Environment::Node));
        assert_eq!(out.blocks["b"].placement, Some(Environment::Node));
    }

    #[test]
    fn unknown_block_type_is_rejected() {
        let doc = doc_from(
            r#"{
                "name": "bad",
                "environments": ["node"],
                "blocks": {
                    "mystery": {"type": "NoSuchType"}
                },
                "wires": []
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::UnknownType { .. }));
    }
}
