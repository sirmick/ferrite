//! Registry-dependent validation + scheduler port-type population.
//!
//! Builds on [`validate_doc`](crate::validate::validate_doc) with the
//! phases that need to see a `BlockSpec`:
//!
//!   * `env_match`      — the declared block type is registered and its
//!                        placement lets it run in the chosen environment
//!   * `wire_endpoints` — port existence on each wire end (block
//!                        existence was caught earlier, without the
//!                        registry)
//!   * `wire_type_match`— `PortType` on both ends of a wire agrees
//!
//! Block **construction** (producing `Box<dyn Block>` instances from
//! declared params) lives in [`crate::block_registry::instantiate_blocks`]
//! — it keys off the same registry but is called separately so tests can
//! exercise validation without also pulling in every block crate.

use std::collections::BTreeMap;

use ferrite_blocks::{BlockSpec, Placement, PortType};

use crate::doc::{Environment, FlowgraphDoc};
use crate::schedule::{InputSource, Schedule, WirePlan};
use crate::validate::{
    split_endpoint, FlowgraphValidationError, Phase, ValidatedDoc, ValidationError,
};

/// The runtime's structural view of a block registry. Implementors resolve
/// a type name to its static [`BlockSpec`]. The TS counterpart is the
/// `BlockRegistryLike` interface — same idea, same contract.
///
/// A real registry (e.g. `ferrite_blocks::registry`) supplies this via a
/// thin adapter in a later commit; tests supply a hand-rolled stub so the
/// runtime crate can be tested without depending on concrete blocks.
pub trait SpecRegistry {
    fn get(&self, type_name: &str) -> Option<BlockSpec>;
    fn has(&self, type_name: &str) -> bool {
        self.get(type_name).is_some()
    }
}

/// Resolved per-id `BlockSpec` map. Produced by [`instantiate_flowgraph`]
/// once registry-dependent validation has passed. `BlockSpec` is `Copy`
/// with all-static fields, so owning rather than borrowing here keeps the
/// trait implementable without `Lazy` caches — see the comment on
/// [`SpecRegistry::get`].
pub type SpecMap = BTreeMap<String, BlockSpec>;

/// Run every registry-dependent validation phase. On success, returns a
/// [`Schedule`] whose `InputSource.port_type` is populated and a per-id
/// [`SpecMap`] the tick pump (and the block constructor) will consume.
///
/// Fails with [`FlowgraphValidationError`] accumulating every problem so
/// a graph author sees them all at once. Block construction is deferred —
/// this function never touches block state.
pub fn instantiate_flowgraph<R: SpecRegistry>(
    v: &ValidatedDoc<'_>,
    registry: &R,
    environment: Environment,
) -> Result<(SpecMap, Schedule), FlowgraphValidationError> {
    let doc = v.doc;

    if !doc.environments.contains(&environment) {
        return Err(FlowgraphValidationError::from_single(ValidationError {
            phase: Phase::EnvMatch,
            error: format!(
                "flowgraph {:?} does not declare environment {:?} (declared: {:?})",
                doc.name, environment, doc.environments
            ),
            wire: None,
            block: None,
            port: None,
        }));
    }

    let mut errors = Vec::new();
    errors.extend(check_env_match(doc, registry, environment));
    errors.extend(check_wire_port_endpoints(doc, registry));
    errors.extend(check_wire_type_match(doc, registry));
    if !errors.is_empty() {
        return Err(FlowgraphValidationError::new(errors));
    }

    let mut specs: SpecMap = BTreeMap::new();
    for (id, decl) in &doc.blocks {
        // env_match would have errored otherwise.
        if let Some(spec) = registry.get(&decl.type_name) {
            specs.insert(id.clone(), spec);
        }
    }

    let schedule = build_schedule_with_types(doc, &specs)
        .map_err(|e| FlowgraphValidationError::from_single(e))?;
    Ok((specs, schedule))
}

// ---------------------------------------------------------------------------
// env_match — type exists + placement compatible.
// ---------------------------------------------------------------------------

fn check_env_match<R: SpecRegistry>(
    doc: &FlowgraphDoc,
    registry: &R,
    env: Environment,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (id, decl) in &doc.blocks {
        let Some(spec) = registry.get(&decl.type_name) else {
            errors.push(ValidationError {
                phase: Phase::EnvMatch,
                error: format!("block type {:?} is not registered", decl.type_name),
                wire: None,
                block: Some(id.clone()),
                port: None,
            });
            continue;
        };
        if !placement_fits(spec.placement, env) {
            errors.push(ValidationError {
                phase: Phase::EnvMatch,
                error: format!(
                    "block type {:?} is {:?}, cannot run in {:?}",
                    decl.type_name, spec.placement, env
                ),
                wire: None,
                block: Some(id.clone()),
                port: None,
            });
        }
    }
    errors
}

fn placement_fits(placement: Placement, env: Environment) -> bool {
    match (placement, env) {
        (Placement::Either, _) => true,
        (Placement::NativeOnly, Environment::Node) => true,
        (Placement::WasmOnly, Environment::Browser) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// wire_endpoints (registry-side): port name exists on the block's spec.
// ---------------------------------------------------------------------------

fn check_wire_port_endpoints<R: SpecRegistry>(
    doc: &FlowgraphDoc,
    registry: &R,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for wire in &doc.wires {
        if let Some(err) = resolve_port_existence(wire, &wire.src, "output", doc, registry) {
            errors.push(err);
        }
        if let Some(err) = resolve_port_existence(wire, &wire.dst, "input", doc, registry) {
            errors.push(err);
        }
    }
    errors
}

fn resolve_port_existence<R: SpecRegistry>(
    wire: &crate::doc::Wire,
    endpoint: &str,
    side_name: &'static str,
    doc: &FlowgraphDoc,
    registry: &R,
) -> Option<ValidationError> {
    let (block_id, port_name) = split_endpoint(endpoint);
    let decl = doc.blocks.get(block_id)?;
    let spec = registry.get(&decl.type_name)?;
    let ports = if side_name == "input" {
        spec.inputs
    } else {
        spec.outputs
    };
    if ports.iter().any(|p| p.name == port_name) {
        return None;
    }
    Some(ValidationError {
        phase: Phase::WireEndpoints,
        error: format!(
            "block {:?} ({}) has no {side_name} port {:?}",
            block_id, decl.type_name, port_name
        ),
        wire: Some(wire.clone()),
        block: Some(block_id.to_string()),
        port: Some(port_name.to_string()),
    })
}

// ---------------------------------------------------------------------------
// wire_type_match — PortType on both ends agrees.
// ---------------------------------------------------------------------------

fn check_wire_type_match<R: SpecRegistry>(
    doc: &FlowgraphDoc,
    registry: &R,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for wire in &doc.wires {
        let Some(src_type) = lookup_port_type(&wire.src, true, doc, registry) else {
            continue;
        };
        let Some(dst_type) = lookup_port_type(&wire.dst, false, doc, registry) else {
            continue;
        };
        if src_type != dst_type {
            errors.push(ValidationError {
                phase: Phase::WireTypeMatch,
                error: format!(
                    "{} is {src_type:?}; {} expects {dst_type:?}",
                    wire.src, wire.dst
                ),
                wire: Some(wire.clone()),
                block: None,
                port: None,
            });
        }
    }
    errors
}

fn lookup_port_type<R: SpecRegistry>(
    endpoint: &str,
    is_output: bool,
    doc: &FlowgraphDoc,
    registry: &R,
) -> Option<PortType> {
    let (block_id, port_name) = split_endpoint(endpoint);
    let decl = doc.blocks.get(block_id)?;
    let spec = registry.get(&decl.type_name)?;
    let ports = if is_output { spec.outputs } else { spec.inputs };
    ports
        .iter()
        .find(|p| p.name == port_name)
        .map(|p| p.port_type)
}

// ---------------------------------------------------------------------------
// Scheduler with port types.
// ---------------------------------------------------------------------------

fn build_schedule_with_types(
    doc: &FlowgraphDoc,
    specs: &SpecMap,
) -> Result<Schedule, ValidationError> {
    let order = crate::schedule::topological_order(doc).map_err(|e| ValidationError {
        phase: Phase::Dag,
        error: format!("scheduler: {e}"),
        wire: None,
        block: None,
        port: None,
    })?;
    let wire_plan = build_wire_plan_with_types(doc, specs);
    Ok(Schedule { order, wire_plan })
}

fn build_wire_plan_with_types(doc: &FlowgraphDoc, specs: &SpecMap) -> WirePlan {
    let mut plan: WirePlan = doc
        .blocks
        .keys()
        .map(|k| (k.clone(), BTreeMap::new()))
        .collect();
    for wire in &doc.wires {
        let (src_block, src_port) = split_endpoint(&wire.src);
        let (dst_block, dst_port) = split_endpoint(&wire.dst);
        let port_type = specs
            .get(src_block)
            .and_then(|s| s.outputs.iter().find(|p| p.name == src_port))
            .map(|p| p.port_type);
        plan.get_mut(dst_block)
            .expect("dst block present after validation")
            .insert(
                dst_port.to_string(),
                InputSource {
                    source_block: src_block.to_string(),
                    source_port: src_port.to_string(),
                    port_type,
                },
            );
    }
    plan
}

// FlowgraphValidationError convenience — only used here; keeping it near
// the call sites avoids widening its public surface.
impl FlowgraphValidationError {
    pub(crate) fn from_single(e: ValidationError) -> Self {
        FlowgraphValidationError::new(vec![e])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_doc;
    use ferrite_blocks::{ParamKind, ParamSpec, PortSpec};

    /// Test-local stub registry. The real `SpecRegistry` adapter over
    /// `ferrite_blocks::registry` lives in `crate::block_registry`; this
    /// stub exists so runtime tests can pin arbitrary block shapes
    /// without depending on any concrete block impls.
    struct StubRegistry(Vec<(&'static str, &'static BlockSpec)>);

    impl SpecRegistry for StubRegistry {
        fn get(&self, type_name: &str) -> Option<BlockSpec> {
            self.0
                .iter()
                .find(|(n, _)| *n == type_name)
                .map(|(_, s)| **s)
        }
    }

    // Reusable static specs — stored as `&'static BlockSpec` internally;
    // the trait surface hands out owned copies (`BlockSpec: Copy`).
    const SRC_PORTS_OUT: &[PortSpec] = &[PortSpec {
        name: "out",
        port_type: PortType::IqF32,
    }];
    const SINK_PORTS_IN: &[PortSpec] = &[PortSpec {
        name: "in",
        port_type: PortType::IqF32,
    }];
    const AUDIO_PORTS_IN: &[PortSpec] = &[PortSpec {
        name: "in",
        port_type: PortType::RealF32,
    }];
    const NO_PORTS: &[PortSpec] = &[];
    const NO_PARAMS: &[ParamSpec] = &[];
    const _PARAM_EXAMPLE: ParamKind = ParamKind::Toggle { default: false };

    const SRC: BlockSpec = BlockSpec {
        type_name: "Src",
        placement: Placement::Either,
        inputs: NO_PORTS,
        outputs: SRC_PORTS_OUT,
        params: NO_PARAMS,
    };
    const SINK: BlockSpec = BlockSpec {
        type_name: "Sink",
        placement: Placement::Either,
        inputs: SINK_PORTS_IN,
        outputs: NO_PORTS,
        params: NO_PARAMS,
    };
    const AUDIO: BlockSpec = BlockSpec {
        type_name: "Audio",
        placement: Placement::WasmOnly,
        inputs: AUDIO_PORTS_IN,
        outputs: NO_PORTS,
        params: NO_PARAMS,
    };
    const SOAPY: BlockSpec = BlockSpec {
        type_name: "Soapy",
        placement: Placement::NativeOnly,
        inputs: NO_PORTS,
        outputs: SRC_PORTS_OUT,
        params: NO_PARAMS,
    };

    fn stub() -> StubRegistry {
        StubRegistry(vec![
            ("Src", &SRC),
            ("Sink", &SINK),
            ("Audio", &AUDIO),
            ("Soapy", &SOAPY),
        ])
    }

    fn doc_from(json: &str) -> FlowgraphDoc {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn instantiates_clean_graph_and_populates_port_types() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"Src"},"sink":{"type":"Sink"}},
                "wires":[["src.out","sink.in"]]
            }"#,
        );
        let v = validate_doc(&d).unwrap();
        let (specs, schedule) = instantiate_flowgraph(&v, &stub(), Environment::Browser).unwrap();
        assert!(specs.contains_key("src") && specs.contains_key("sink"));
        let sink_in = &schedule.wire_plan["sink"]["in"];
        assert_eq!(sink_in.port_type, Some(PortType::IqF32));
    }

    #[test]
    fn rejects_unknown_type() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"a":{"type":"NotReal"},"sink":{"type":"Sink"}},
                "wires":[["a.out","sink.in"]]
            }"#,
        );
        let v = validate_doc(&d).unwrap();
        let err = instantiate_flowgraph(&v, &stub(), Environment::Browser).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::EnvMatch));
    }

    #[test]
    fn rejects_placement_mismatch() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser","node"],
                "blocks":{"src":{"type":"Soapy"},"sink":{"type":"Audio"}},
                "wires":[["src.out","sink.in"]]
            }"#,
        );
        let v = validate_doc(&d).unwrap();
        // Soapy is native_only — cannot run in browser.
        let err = instantiate_flowgraph(&v, &stub(), Environment::Browser).unwrap_err();
        assert!(err
            .errors
            .iter()
            .any(|e| e.phase == Phase::EnvMatch && e.block.as_deref() == Some("src")));
    }

    #[test]
    fn rejects_unknown_port() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"Src"},"sink":{"type":"Sink"}},
                "wires":[["src.nope","sink.in"]]
            }"#,
        );
        let v = validate_doc(&d).unwrap();
        let err = instantiate_flowgraph(&v, &stub(), Environment::Browser).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::WireEndpoints));
    }

    #[test]
    fn rejects_wire_type_mismatch() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"Src"},"audio":{"type":"Audio"}},
                "wires":[["src.out","audio.in"]]
            }"#,
        );
        // Src emits IqF32, Audio expects RealF32 — type mismatch.
        let v = validate_doc(&d).unwrap();
        let err = instantiate_flowgraph(&v, &stub(), Environment::Browser).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::WireTypeMatch));
    }

    #[test]
    fn rejects_env_not_declared() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"Src"},"sink":{"type":"Sink"}},
                "wires":[["src.out","sink.in"]]
            }"#,
        );
        let v = validate_doc(&d).unwrap();
        let err = instantiate_flowgraph(&v, &stub(), Environment::Node).unwrap_err();
        assert!(err.errors.iter().any(|e| e.phase == Phase::EnvMatch));
    }
}
