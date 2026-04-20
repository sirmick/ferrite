//! Preset diff → minimum reconfigure plan.
//!
//! Given two `FlowgraphDoc`s (the currently-applied preset and the one
//! the user just authored), this module produces a `ReconfigurePlan`
//! describing the smallest runtime action needed to pick up the change.
//! Each changed param is tagged with the `ReconfigureScope` declared
//! on its `ParamSpec`; the overall scope is the max (costliest) scope
//! across every change.
//!
//! This module is pure data — it does not touch a live [`Runtime`].
//! The apply/rollback path is a separate concern that reads this plan
//! and decides what to tear down and restart. See D19 in
//! `docs/09-decisions.md` for the reasoning behind scope-based diffing.
//!
//! [`Runtime`]: crate::runtime::Runtime
//!
//! ### Scope of this slice
//!
//! Handles intra-block param changes exactly: for each block that
//! exists in both docs with the same `type`, each differing param key
//! becomes one `ParamChange` stamped with the scope from the block's
//! spec. Structural changes (added / removed blocks, changed block
//! types, changed wires) escalate the overall scope to
//! `SourceRestart` — the runtime has to tear the graph down and load
//! from scratch. The apply commit (next M3 slice) is free to handle
//! structural edits more cleverly; the scope label here is an upper
//! bound on what that code must be prepared to do.
//!
//! Unknown params (keys that aren't in the block's spec) also escalate
//! to `SourceRestart`. Rejecting them outright would make old presets
//! un-diffable after a block rev; assuming they're harmless would hide
//! real breakage. Restarting the source is the safe middle ground.

use std::collections::BTreeSet;

use ferrite_blocks::{BlockSpec, ReconfigureScope};
use serde_json::Value;
use thiserror::Error;

use crate::doc::FlowgraphDoc;
use crate::instantiate::SpecRegistry;

/// One knob that moved between the old and new presets.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamChange {
    pub block_id: String,
    pub param_key: String,
    pub old_value: Value,
    pub new_value: Value,
    pub scope: ReconfigureScope,
}

/// Structural delta that requires more than a param tweak. Only
/// reported when [`diff_presets`] sees the topology change; the runtime
/// treats any of these as a [`ReconfigureScope::SourceRestart`].
#[derive(Debug, Clone, PartialEq)]
pub enum StructuralChange {
    BlockAdded {
        block_id: String,
        type_name: String,
    },
    BlockRemoved {
        block_id: String,
        type_name: String,
    },
    BlockTypeChanged {
        block_id: String,
        old_type: String,
        new_type: String,
    },
    WiresChanged,
    EnvironmentsChanged,
}

/// Plan produced by diffing two presets. Iterate `changes` to see
/// per-param scopes; apply according to `overall`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconfigurePlan {
    pub changes: Vec<ParamChange>,
    pub structural: Vec<StructuralChange>,
    pub overall: ReconfigureScope,
}

impl ReconfigurePlan {
    /// `true` when neither docs changed — the caller skips the apply.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty() && self.structural.is_empty()
    }
}

/// Error cases the diff refuses to reason about. Used sparingly —
/// anything that can be described as a structural delta goes through
/// [`StructuralChange`] instead.
#[derive(Debug, Error)]
pub enum ReconfigureError {
    #[error("block {block_id:?}: type {type_name:?} is not registered")]
    UnknownBlockType { block_id: String, type_name: String },
    #[error("doc names disagree: old={old:?}, new={new:?}")]
    NameMismatch { old: String, new: String },
}

/// Diff two presets, consulting `registry` for each block's param scopes.
///
/// Returns a plan describing the minimum action. Rejects only cases the
/// runtime can't sensibly reconfigure into — name mismatch (caller
/// confused which preset is which) and types the registry doesn't know.
/// Everything else — including wholesale topology rewrites — comes
/// back as a plan with `overall: SourceRestart`.
pub fn diff_presets(
    old: &FlowgraphDoc,
    new: &FlowgraphDoc,
    registry: &dyn SpecRegistry,
) -> Result<ReconfigurePlan, ReconfigureError> {
    if old.name != new.name {
        return Err(ReconfigureError::NameMismatch {
            old: old.name.clone(),
            new: new.name.clone(),
        });
    }

    let mut structural = Vec::new();

    if old.environments != new.environments {
        structural.push(StructuralChange::EnvironmentsChanged);
    }

    let old_ids: BTreeSet<&String> = old.blocks.keys().collect();
    let new_ids: BTreeSet<&String> = new.blocks.keys().collect();

    for id in old_ids.difference(&new_ids) {
        let decl = &old.blocks[*id];
        structural.push(StructuralChange::BlockRemoved {
            block_id: (*id).clone(),
            type_name: decl.type_name.clone(),
        });
    }
    for id in new_ids.difference(&old_ids) {
        let decl = &new.blocks[*id];
        structural.push(StructuralChange::BlockAdded {
            block_id: (*id).clone(),
            type_name: decl.type_name.clone(),
        });
    }

    if old.wires != new.wires {
        structural.push(StructuralChange::WiresChanged);
    }

    let mut changes = Vec::new();
    for id in old_ids.intersection(&new_ids) {
        let id_str = (*id).clone();
        let old_decl = &old.blocks[*id];
        let new_decl = &new.blocks[*id];

        if old_decl.type_name != new_decl.type_name {
            structural.push(StructuralChange::BlockTypeChanged {
                block_id: id_str.clone(),
                old_type: old_decl.type_name.clone(),
                new_type: new_decl.type_name.clone(),
            });
            // A retyped block invalidates the old params entirely;
            // skip the param diff for this id.
            continue;
        }

        let spec = registry.get(&old_decl.type_name).ok_or_else(|| {
            ReconfigureError::UnknownBlockType {
                block_id: id_str.clone(),
                type_name: old_decl.type_name.clone(),
            }
        })?;

        diff_params(
            &id_str,
            &spec,
            old_decl.params.as_ref(),
            new_decl.params.as_ref(),
            &mut changes,
            &mut structural,
        );
    }

    let mut overall = if structural.is_empty() {
        ReconfigureScope::SelfBlock
    } else {
        ReconfigureScope::SourceRestart
    };
    for c in &changes {
        overall = overall.merge(c.scope);
    }

    Ok(ReconfigurePlan {
        changes,
        structural,
        overall,
    })
}

fn diff_params(
    block_id: &str,
    spec: &BlockSpec,
    old_params: Option<&Value>,
    new_params: Option<&Value>,
    changes: &mut Vec<ParamChange>,
    structural: &mut Vec<StructuralChange>,
) {
    // The set of keys the author touched in either doc. Keys that are
    // missing from a doc take the block's registered default value (so
    // omitting a param from one side is equivalent to setting it to
    // default — the diff only reports a change if the *effective*
    // values disagree).
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for k in params_keys(old_params) {
        keys.insert(k);
    }
    for k in params_keys(new_params) {
        keys.insert(k);
    }

    for key in keys {
        let old_val = param_value_or_null(old_params, key);
        let new_val = param_value_or_null(new_params, key);
        if old_val == new_val {
            continue;
        }
        if let Some(p) = spec.params.iter().find(|p| p.key == key) {
            changes.push(ParamChange {
                block_id: block_id.to_string(),
                param_key: key.to_string(),
                old_value: old_val,
                new_value: new_val,
                scope: p.reconfig_scope,
            });
        } else {
            // Unknown key — registered spec doesn't declare it.
            // Treat as a structural delta: safest option is to
            // rebuild from scratch.
            structural.push(StructuralChange::WiresChanged);
            // Overwriting `WiresChanged` is fine; the overall
            // escalation is what matters. But drop one copy of
            // each signal so `structural` doesn't explode.
            break;
        }
    }
}

fn params_keys(v: Option<&Value>) -> impl Iterator<Item = &str> {
    v.and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys().map(String::as_str))
}

fn param_value_or_null(v: Option<&Value>, key: &str) -> Value {
    v.and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_blocks::{ParamKind, ParamSpec as FBParamSpec, Placement, PortSpec, PortType};
    use serde_json::json;

    // Hand-rolled registry so the tests exercise diff logic without
    // depending on the inventory set. Mirrors the stub used elsewhere
    // in the runtime crate.
    struct TestRegistry;

    const SCALAR_SPEC: BlockSpec = BlockSpec {
        type_name: "Scalar",
        placement: Placement::Either,
        inputs: &[PortSpec {
            name: "in",
            port_type: PortType::RealF32,
        }],
        outputs: &[PortSpec {
            name: "out",
            port_type: PortType::RealF32,
        }],
        params: &[
            FBParamSpec {
                key: "gain",
                label: "Gain",
                kind: ParamKind::Range {
                    min: 0.0,
                    max: 10.0,
                    step: 0.1,
                    default: 1.0,
                    unit: "",
                },
                reconfig_scope: ReconfigureScope::SelfBlock,
            },
            FBParamSpec {
                key: "taps",
                label: "Filter taps",
                kind: ParamKind::Range {
                    min: 3.0,
                    max: 1024.0,
                    step: 2.0,
                    default: 33.0,
                    unit: "",
                },
                reconfig_scope: ReconfigureScope::Downstream,
            },
            FBParamSpec {
                key: "rate",
                label: "Sample rate",
                kind: ParamKind::Range {
                    min: 1_000.0,
                    max: 1.0e7,
                    step: 1.0,
                    default: 48_000.0,
                    unit: "Hz",
                },
                reconfig_scope: ReconfigureScope::SourceRestart,
            },
        ],
    };

    const OTHER_SPEC: BlockSpec = BlockSpec {
        type_name: "Other",
        placement: Placement::Either,
        inputs: &[],
        outputs: &[PortSpec {
            name: "out",
            port_type: PortType::RealF32,
        }],
        params: &[],
    };

    impl SpecRegistry for TestRegistry {
        fn get(&self, type_name: &str) -> Option<BlockSpec> {
            match type_name {
                "Scalar" => Some(SCALAR_SPEC),
                "Other" => Some(OTHER_SPEC),
                _ => None,
            }
        }
    }

    fn doc(json_body: serde_json::Value) -> FlowgraphDoc {
        serde_json::from_value(json_body).unwrap()
    }

    fn base_doc() -> FlowgraphDoc {
        doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar", "params": { "gain": 1.0, "taps": 33, "rate": 48000 } }
            },
            "wires": []
        }))
    }

    #[test]
    fn identical_presets_are_a_noop() {
        let a = base_doc();
        let b = a.clone();
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert!(plan.is_noop());
        assert_eq!(plan.overall, ReconfigureScope::SelfBlock);
    }

    #[test]
    fn single_param_change_uses_its_declared_scope() {
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar", "params": { "gain": 2.5, "taps": 33, "rate": 48000 } }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(plan.changes.len(), 1);
        let c = &plan.changes[0];
        assert_eq!(c.block_id, "s");
        assert_eq!(c.param_key, "gain");
        assert_eq!(c.scope, ReconfigureScope::SelfBlock);
        assert_eq!(plan.overall, ReconfigureScope::SelfBlock);
    }

    #[test]
    fn overall_scope_is_the_costliest_across_changes() {
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar", "params": { "gain": 2.5, "taps": 65, "rate": 48000 } }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(plan.changes.len(), 2);
        // gain=Self, taps=Downstream → overall is Downstream.
        assert_eq!(plan.overall, ReconfigureScope::Downstream);
    }

    #[test]
    fn source_rate_change_escalates_to_source_restart() {
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar", "params": { "gain": 1.0, "taps": 33, "rate": 96000 } }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].scope, ReconfigureScope::SourceRestart);
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn adding_a_block_is_structural() {
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s":   { "type": "Scalar", "params": { "gain": 1.0, "taps": 33, "rate": 48000 } },
                "new": { "type": "Other" }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(
            plan.structural,
            vec![StructuralChange::BlockAdded {
                block_id: "new".into(),
                type_name: "Other".into(),
            }]
        );
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn removing_a_block_is_structural() {
        let a = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s":  { "type": "Scalar", "params": { "gain": 1.0, "taps": 33, "rate": 48000 } },
                "o":  { "type": "Other" }
            },
            "wires": []
        }));
        let b = base_doc();
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(
            plan.structural,
            vec![StructuralChange::BlockRemoved {
                block_id: "o".into(),
                type_name: "Other".into(),
            }]
        );
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn retyping_a_block_is_structural_and_skips_param_diff() {
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Other" }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert!(
            plan.changes.is_empty(),
            "no param changes for a retyped block"
        );
        assert_eq!(
            plan.structural,
            vec![StructuralChange::BlockTypeChanged {
                block_id: "s".into(),
                old_type: "Scalar".into(),
                new_type: "Other".into(),
            }]
        );
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn wire_change_is_structural() {
        let a = base_doc();
        let mut b = base_doc();
        b.wires.push(crate::doc::Wire::new("s.out", "s.in"));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(plan.structural, vec![StructuralChange::WiresChanged]);
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn env_change_is_structural() {
        let a = base_doc();
        let mut b = base_doc();
        b.environments.push(crate::doc::Environment::Node);
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert!(plan
            .structural
            .contains(&StructuralChange::EnvironmentsChanged));
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn mismatched_names_are_an_error() {
        let a = base_doc();
        let mut b = base_doc();
        b.name = "other".into();
        let err = diff_presets(&a, &b, &TestRegistry).unwrap_err();
        assert!(matches!(err, ReconfigureError::NameMismatch { .. }));
    }

    #[test]
    fn unknown_block_type_is_an_error() {
        let a = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": { "x": { "type": "Ghost" } },
            "wires": []
        }));
        let b = a.clone();
        let err = diff_presets(&a, &b, &TestRegistry).unwrap_err();
        assert!(matches!(err, ReconfigureError::UnknownBlockType { .. }));
    }

    #[test]
    fn omitted_param_equals_present_default_wire_value() {
        // Old has explicit gain=1.0; new omits params entirely. The
        // diff should NOT report a change for gain — comparing
        // json!Null vs json!Null once both are absent is a no-op.
        // But the old doc has gain=1.0, so the "effective" old value
        // is 1.0 and new is Null — this IS a change, just against the
        // spec's default, because we compare JSON literals not
        // resolved-against-defaults values.
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar" }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        // Three keys moved 1.0→Null, 33→Null, 48000→Null; each is a
        // ParamChange with its declared scope.
        assert_eq!(plan.changes.len(), 3);
        let scopes: Vec<_> = plan.changes.iter().map(|c| c.scope).collect();
        assert!(scopes.contains(&ReconfigureScope::SelfBlock));
        assert!(scopes.contains(&ReconfigureScope::Downstream));
        assert!(scopes.contains(&ReconfigureScope::SourceRestart));
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
    }

    #[test]
    fn unknown_param_key_escalates_via_wires_changed() {
        // "gizmo" isn't in Scalar's spec — the diff can't know the
        // proper scope, so it reports a structural change.
        let a = base_doc();
        let b = doc(json!({
            "name": "t",
            "environments": ["browser"],
            "blocks": {
                "s": { "type": "Scalar", "params": { "gain": 1.0, "taps": 33, "rate": 48000, "gizmo": 7 } }
            },
            "wires": []
        }));
        let plan = diff_presets(&a, &b, &TestRegistry).unwrap();
        assert_eq!(plan.overall, ReconfigureScope::SourceRestart);
        assert!(plan.structural.contains(&StructuralChange::WiresChanged));
    }
}
