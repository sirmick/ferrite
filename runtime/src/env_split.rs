//! Environment split — carves a cross-env preset into the single-env
//! subgraph that one runtime instance will actually run.
//!
//! A preset is authored as one doc. Blocks are placed in either the
//! native ("node") half or the browser half via `placement` on each
//! instance; blocks whose `BlockSpec::placement` is `NativeOnly` or
//! `WasmOnly` inherit that side automatically. `Placement::Either`
//! blocks inherit from adjacent wired neighbours when possible — the
//! author only has to pin one end of an Either chain explicitly.
//!
//! Wires that cross the boundary are replaced at split time with a
//! `WsBridgeTx`/`WsBridgeRx` pair: the producing side keeps its block
//! and gets a new Tx consuming the signal for transport; the consuming
//! side gets a new Rx producing the signal and keeps its own consumer.
//!
//! A `"ui:<name>"` destination sentinel terminates a stream in the UI
//! rather than in another block: the producing side gets a `WsBridgeTx`
//! tagged with the `ui_name`; the consuming side drops the wire entirely
//! (the browser reads the stream off the WebSocket directly).
//!
//! Stream IDs are allocated deterministically from the doc's wire list
//! (in input order, counting every boundary-crossing wire — including
//! `ui:<name>`) so both halves of the split agree on which Tx pairs with
//! which Rx without any out-of-band channel.
//!
//! Today only `node → browser` crossings are supported. The receive-
//! side block on native (`WsBridgeTx`'s mirror) does not exist yet;
//! browser-to-server dataflow is an M3+ concern when reconfigure events
//! and control channels land.

use std::collections::BTreeMap;

use ferrite_blocks::{Placement, PortType};
use serde_json::{json, Value};
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
        "block {id:?} of type {type_name:?} is Placement::Either and has no explicit `placement` nor any wired neighbour with a known placement — pin it directly"
    )]
    PlacementUnresolved { id: String, type_name: String },
    #[error(
        "wire {wire:?} crosses from browser to node; only node→browser crossings are supported today"
    )]
    UnsupportedCrossing { wire: Wire },
    #[error("split would synthesize bridge block {id:?} but the source doc already uses that id")]
    BridgeNameCollision { id: String },
    #[error(
        "ui:{ui_name:?} wire source {endpoint:?} has port type {port_type:?} — no WsBridgeTx variant exists for it"
    )]
    UnsupportedUiPortType {
        ui_name: String,
        endpoint: String,
        port_type: PortType,
    },
    #[error(
        "ui:{ui_name:?} wire source {endpoint:?} references an output port that doesn't exist"
    )]
    UnknownSourcePort { ui_name: String, endpoint: String },
}

/// Split `doc` for `env`: keep only blocks placed in `env`, keep only
/// wires internal to `env`, and terminate each boundary-crossing wire
/// with a synthesized `WsBridgeTx` (on the node half) or `WsBridgeRx`
/// (on the browser half). The two halves produced by calling this with
/// both environments share stream IDs so their bridge pairs line up.
pub fn split_for_environment(
    doc: &FlowgraphDoc,
    env: Environment,
    registry: &dyn SpecRegistry,
) -> Result<FlowgraphDoc, SplitError> {
    let placements = resolve_placements(doc, registry)?;
    validate_crossings(doc, &placements)?;

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
        // `ui:<name>` wires always produce on the source's env and drop
        // on the other. The slot is consumed either way so numbering
        // agrees across halves.
        if let Some(ui_name) = wire.ui_sink_name() {
            let (src_block, _) = split_endpoint(&wire.src);
            let producer_env = placements[src_block];
            let sid = CROSS_ENV_STREAM_BASE + crossing_index;
            crossing_index += 1;
            if producer_env != env {
                continue;
            }
            let tx_type = pick_ui_tx_type(doc, registry, &wire.src, ui_name)?;
            let bridge_id = format!("__ui_{ui_name}_{sid}");
            insert_bridge(
                &mut new_blocks,
                doc,
                bridge_id.clone(),
                tx_type,
                env,
                json!({ "stream_id": sid, "ui_name": ui_name }),
            )?;
            new_wires.push(Wire::new(wire.src.clone(), format!("{bridge_id}.in")));
            continue;
        }

        let (src_block, _) = split_endpoint(&wire.src);
        let (dst_block, _) = split_endpoint(&wire.dst);
        let src_env = placements[src_block];
        let dst_env = placements[dst_block];

        match (src_env == env, dst_env == env) {
            (true, true) => new_wires.push(wire.clone()),
            (true, false) => {
                let sid = CROSS_ENV_STREAM_BASE + crossing_index;
                let bridge_id = format!("__bridge_tx_{sid}");
                insert_bridge(
                    &mut new_blocks,
                    doc,
                    bridge_id.clone(),
                    "WsBridgeTx",
                    env,
                    json!({ "stream_id": sid }),
                )?;
                new_wires.push(Wire::new(wire.src.clone(), format!("{bridge_id}.in")));
                crossing_index += 1;
            }
            (false, true) => {
                let sid = CROSS_ENV_STREAM_BASE + crossing_index;
                let bridge_id = format!("__bridge_rx_{sid}");
                insert_bridge(
                    &mut new_blocks,
                    doc,
                    bridge_id.clone(),
                    "WsBridgeRx",
                    env,
                    json!({ "stream_id": sid }),
                )?;
                new_wires.push(Wire::new(format!("{bridge_id}.out"), wire.dst.clone()));
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

/// Resolve the source port's type and map it to the matching WsBridgeTx
/// variant. Only the two Tx variants that exist today are supported
/// (IqF32 → `WsBridgeTx`, FftU8 → `WsBridgeTxFftU8`); other port types
/// would need their own Tx block before a `ui:<name>` wire can carry them.
fn pick_ui_tx_type(
    doc: &FlowgraphDoc,
    registry: &dyn SpecRegistry,
    source: &str,
    ui_name: &str,
) -> Result<&'static str, SplitError> {
    let (block_id, port_name) = split_endpoint(source);
    let decl = doc
        .blocks
        .get(block_id)
        .expect("validate_doc caught unknown source blocks before split");
    let spec = registry
        .get(&decl.type_name)
        .expect("resolve_placements caught unknown block types before split");
    let port =
        spec.outputs
            .iter()
            .find(|p| p.name == port_name)
            .ok_or(SplitError::UnknownSourcePort {
                ui_name: ui_name.to_string(),
                endpoint: source.to_string(),
            })?;
    match port.port_type {
        PortType::IqF32 => Ok("WsBridgeTx"),
        PortType::FftU8 => Ok("WsBridgeTxFftU8"),
        other => Err(SplitError::UnsupportedUiPortType {
            ui_name: ui_name.to_string(),
            endpoint: source.to_string(),
            port_type: other,
        }),
    }
}

/// Reject browser→node crossings up front so the split loop below can
/// assume every crossing wire goes the supported direction.
fn validate_crossings(
    doc: &FlowgraphDoc,
    placements: &BTreeMap<String, Environment>,
) -> Result<(), SplitError> {
    for wire in &doc.wires {
        if wire.ui_sink_name().is_some() {
            continue;
        }
        let (src, _) = split_endpoint(&wire.src);
        let (dst, _) = split_endpoint(&wire.dst);
        if placements[src] == Environment::Browser && placements[dst] == Environment::Node {
            return Err(SplitError::UnsupportedCrossing { wire: wire.clone() });
        }
    }
    Ok(())
}

fn insert_bridge(
    new_blocks: &mut BTreeMap<String, BlockInstanceDecl>,
    source_doc: &FlowgraphDoc,
    id: String,
    type_name: &'static str,
    env: Environment,
    params: Value,
) -> Result<(), SplitError> {
    if new_blocks.contains_key(&id) || source_doc.blocks.contains_key(&id) {
        return Err(SplitError::BridgeNameCollision { id });
    }
    new_blocks.insert(
        id,
        BlockInstanceDecl {
            type_name: type_name.to_string(),
            params: Some(params),
            placement: Some(env),
        },
    );
    Ok(())
}

fn resolve_placements(
    doc: &FlowgraphDoc,
    registry: &dyn SpecRegistry,
) -> Result<BTreeMap<String, Environment>, SplitError> {
    // Pass 1: seed from explicit `placement` + hard-pinned BlockSpec.
    // Unresolved `Placement::Either` (no explicit) stays as `None` and
    // gets filled in by neighbour propagation below.
    let mut out: BTreeMap<String, Option<Environment>> = BTreeMap::new();
    for (id, decl) in &doc.blocks {
        let spec = registry
            .get(&decl.type_name)
            .ok_or_else(|| SplitError::UnknownType {
                id: id.clone(),
                type_name: decl.type_name.clone(),
            })?;
        let resolved = match (decl.placement, spec.placement) {
            (Some(e), Placement::Either) => Some(e),
            (Some(Environment::Node) | None, Placement::NativeOnly) => Some(Environment::Node),
            (Some(Environment::Browser) | None, Placement::WasmOnly) => Some(Environment::Browser),
            (None, Placement::Either) => None,
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

    // Pass 2: propagate placements across wires until fixpoint. Skip
    // `ui:<name>` wires — they're env-crossing by definition, so carry
    // no inference signal. A wire between a known and an unknown block
    // pins the unknown side to match; two unknowns on both ends wait
    // for the next pass.
    loop {
        let mut changed = false;
        for wire in &doc.wires {
            if wire.ui_sink_name().is_some() {
                continue;
            }
            let (src, _) = split_endpoint(&wire.src);
            let (dst, _) = split_endpoint(&wire.dst);
            let src_placement = out.get(src).copied().flatten();
            let dst_placement = out.get(dst).copied().flatten();
            match (src_placement, dst_placement) {
                (Some(e), None) => {
                    out.insert(dst.to_string(), Some(e));
                    changed = true;
                }
                (None, Some(e)) => {
                    out.insert(src.to_string(), Some(e));
                    changed = true;
                }
                _ => {}
            }
        }
        if !changed {
            break;
        }
    }

    // Pass 3: any still-None means no explicit pin and no wired neighbour
    // ever resolved — author must pin directly.
    let mut final_map = BTreeMap::new();
    for (id, opt) in out {
        let Some(e) = opt else {
            let decl = &doc.blocks[&id];
            return Err(SplitError::PlacementUnresolved {
                id,
                type_name: decl.type_name.clone(),
            });
        };
        final_map.insert(id, e);
    }
    Ok(final_map)
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
    const FFT_OUT: &[PortSpec] = &[PortSpec {
        name: "out",
        port_type: PortType::FftU8,
    }];
    const FFT_HW_SRC: BlockSpec = BlockSpec {
        type_name: "FftHwSrc",
        placement: Placement::NativeOnly,
        inputs: NO_PORTS,
        outputs: FFT_OUT,
        params: NO_PARAMS,
    };
    const REAL_OUT: &[PortSpec] = &[PortSpec {
        name: "out",
        port_type: PortType::RealF32,
    }];
    const REAL_HW_SRC: BlockSpec = BlockSpec {
        type_name: "RealHwSrc",
        placement: Placement::NativeOnly,
        inputs: NO_PORTS,
        outputs: REAL_OUT,
        params: NO_PARAMS,
    };

    fn stub() -> StubRegistry {
        StubRegistry(vec![
            ("HwSrc", &HW_SRC),
            ("WasmSink", &WASM_SINK),
            ("Either", &EITHER),
            ("WsBridgeTx", &WS_TX),
            ("WsBridgeRx", &WS_RX),
            ("FftHwSrc", &FFT_HW_SRC),
            ("RealHwSrc", &REAL_HW_SRC),
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
            .any(|w| w.src == "src.out" && w.dst == "mid.in"));
        assert!(node
            .wires
            .iter()
            .any(|w| w.src == "mid.out" && w.dst == format!("{bridge_id}.in")));
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
            .any(|w| w.src == format!("{bridge_id}.out") && w.dst == "sink.in"));
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

    // -----------------------------------------------------------------
    // Either-block placement inference from adjacent neighbours.
    // -----------------------------------------------------------------

    #[test]
    fn either_inherits_from_hardware_pinned_neighbour() {
        // src is NativeOnly; mid is Either without explicit placement.
        // The single wire src→mid pins mid to node.
        let doc = doc_from(
            r#"{
                "name": "infer",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  {"type": "HwSrc"},
                    "mid":  {"type": "Either"},
                    "sink": {"type": "WasmSink"}
                },
                "wires": [
                    ["src.out", "mid.in"],
                    ["mid.out", "sink.in"]
                ]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        assert!(node.blocks.contains_key("mid"));
        assert_eq!(node.blocks["mid"].placement, Some(Environment::Node));
        // mid→sink is the crossing (slot 0); src→mid stays internal.
        let tx_id = format!("__bridge_tx_{CROSS_ENV_STREAM_BASE}");
        assert!(node.blocks.contains_key(&tx_id));
    }

    #[test]
    fn either_propagates_along_chain_from_a_single_pin() {
        // Chain of Eithers with one explicit pin at the tail; inference
        // must walk the chain to every Either.
        let doc = doc_from(
            r#"{
                "name": "chain",
                "environments": ["browser"],
                "blocks": {
                    "a": {"type": "Either"},
                    "b": {"type": "Either"},
                    "c": {"type": "Either", "placement": "browser"}
                },
                "wires": [
                    ["a.out", "b.in"],
                    ["b.out", "c.in"]
                ]
            }"#,
        );
        let out = split_for_environment(&doc, Environment::Browser, &stub()).unwrap();
        for id in ["a", "b", "c"] {
            assert_eq!(out.blocks[id].placement, Some(Environment::Browser));
        }
    }

    #[test]
    fn either_isolated_from_any_known_placement_is_still_rejected() {
        // Two Eithers wired together but nothing else pins either side.
        // Inference has nothing to anchor on.
        let doc = doc_from(
            r#"{
                "name": "floating",
                "environments": ["node", "browser"],
                "blocks": {
                    "a": {"type": "Either"},
                    "b": {"type": "Either"}
                },
                "wires": [["a.out", "b.in"]]
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::PlacementUnresolved { .. }));
    }

    // -----------------------------------------------------------------
    // `ui:<name>` destination sentinel.
    // -----------------------------------------------------------------

    #[test]
    fn ui_sink_inserts_tx_with_ui_name_on_producer_half_and_nothing_on_the_other() {
        let doc = doc_from(
            r#"{
                "name": "ui-sink",
                "environments": ["node", "browser"],
                "blocks": {
                    "src": {"type": "HwSrc"}
                },
                "wires": [["src.out", "ui:fft"]]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        let tx_id = format!("__ui_fft_{CROSS_ENV_STREAM_BASE}");
        let tx = node
            .blocks
            .get(&tx_id)
            .expect("ui-side bridge inserted on producer half");
        assert_eq!(tx.type_name, "WsBridgeTx");
        let params = tx.params.as_ref().expect("bridge has params");
        assert_eq!(
            params["stream_id"].as_u64(),
            Some(u64::from(CROSS_ENV_STREAM_BASE))
        );
        assert_eq!(params["ui_name"].as_str(), Some("fft"));
        assert!(node
            .wires
            .iter()
            .any(|w| w.src == "src.out" && w.dst == format!("{tx_id}.in")));

        // Browser half must drop the wire and NOT insert any bridge.
        let browser = split_for_environment(&doc, Environment::Browser, &stub()).unwrap();
        assert!(browser.blocks.is_empty());
        assert!(browser.wires.is_empty());
    }

    #[test]
    fn ui_sink_on_fft_u8_source_synthesizes_fft_u8_tx() {
        let doc = doc_from(
            r#"{
                "name": "ui-fft",
                "environments": ["node", "browser"],
                "blocks": {
                    "src": {"type": "FftHwSrc"}
                },
                "wires": [["src.out", "ui:fft"]]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        let tx_id = format!("__ui_fft_{CROSS_ENV_STREAM_BASE}");
        let tx = node.blocks.get(&tx_id).expect("ui-side bridge inserted");
        assert_eq!(tx.type_name, "WsBridgeTxFftU8");
    }

    #[test]
    fn ui_sink_on_unsupported_port_type_errors() {
        let doc = doc_from(
            r#"{
                "name": "ui-real",
                "environments": ["node", "browser"],
                "blocks": {
                    "src": {"type": "RealHwSrc"}
                },
                "wires": [["src.out", "ui:audio"]]
            }"#,
        );
        let err = split_for_environment(&doc, Environment::Node, &stub()).unwrap_err();
        assert!(matches!(err, SplitError::UnsupportedUiPortType { .. }));
    }

    #[test]
    fn ui_sink_and_cross_env_wires_share_consecutive_stream_ids() {
        // Mix of ui: and plain crossing — both halves must agree on
        // stream_id slot numbering to align bridges.
        let doc = doc_from(
            r#"{
                "name": "ui-and-cross",
                "environments": ["node", "browser"],
                "blocks": {
                    "src1": {"type": "HwSrc"},
                    "src2": {"type": "HwSrc"},
                    "sink": {"type": "WasmSink"}
                },
                "wires": [
                    ["src1.out", "ui:fft"],
                    ["src2.out", "sink.in"]
                ]
            }"#,
        );
        let node = split_for_environment(&doc, Environment::Node, &stub()).unwrap();
        // Slot 0 → ui_fft tx; slot 1 → cross-env tx.
        assert!(node
            .blocks
            .contains_key(&format!("__ui_fft_{CROSS_ENV_STREAM_BASE}")));
        assert!(node
            .blocks
            .contains_key(&format!("__bridge_tx_{}", CROSS_ENV_STREAM_BASE + 1)));

        let browser = split_for_environment(&doc, Environment::Browser, &stub()).unwrap();
        // Browser still counts slot 0 (for ui) but drops it; rx lands on slot 1.
        assert!(browser
            .blocks
            .contains_key(&format!("__bridge_rx_{}", CROSS_ENV_STREAM_BASE + 1)));
    }
}
