//! Adapters binding the runtime to `ferrite_blocks`.
//!
//! [`InventorySpecRegistry`] fulfils [`SpecRegistry`] by looking types up
//! in the static inventory set populated at link time. The companion
//! [`instantiate_blocks`] walks a doc and asks each block's
//! [`ferrite_blocks::BlockFactory`] to produce a runnable instance.
//!
//! Kept apart from [`crate::instantiate`] so the validation/scheduler
//! logic there stays agnostic of any concrete block crate — test code in
//! that module still supplies a hand-rolled stub registry — while this
//! module exercises the real registry end-to-end.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use ferrite_blocks::{registry, Block, BlockSpec};

use crate::doc::FlowgraphDoc;
use crate::instantiate::SpecRegistry;

/// Real registry, backed by the `ferrite_blocks::registry` inventory set.
/// Zero-sized; construct ad-hoc at call sites.
pub struct InventorySpecRegistry;

impl SpecRegistry for InventorySpecRegistry {
    fn get(&self, type_name: &str) -> Option<BlockSpec> {
        registry::find(type_name).map(|e| e.spec())
    }
}

/// Per-id map of constructed, runnable block instances.
pub type BlockMap = BTreeMap<String, Box<dyn Block>>;

/// Instantiate every block declared in `doc` using the inventory registry.
///
/// Callers are expected to have already run
/// [`crate::validate::validate_doc`] and
/// [`crate::instantiate::instantiate_flowgraph`]; this function re-checks
/// type existence so a skipped precondition produces a clean error
/// instead of a panic. Missing `params` defaults via each block's
/// `Default` impl — `BlockFactory::construct` treats `Null` as "use
/// defaults".
pub fn instantiate_blocks(doc: &FlowgraphDoc) -> Result<BlockMap> {
    let mut out: BlockMap = BTreeMap::new();
    for (id, decl) in &doc.blocks {
        let entry = registry::find(&decl.type_name)
            .ok_or_else(|| anyhow!("block {id:?}: type {:?} is not registered", decl.type_name))?;
        let params = decl.params.clone().unwrap_or(serde_json::Value::Null);
        let block = entry
            .construct(&params)
            .with_context(|| format!("constructing block {id:?} ({})", decl.type_name))?;
        out.insert(id.clone(), block);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_from(json: &str) -> FlowgraphDoc {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn inventory_registry_resolves_shipped_block_types() {
        let r = InventorySpecRegistry;
        for t in [
            "SineSource",
            "Decimator",
            "FmDemod",
            "FFT",
            "LogMagU8",
            "Channelizer",
            "FileIqSource",
        ] {
            assert!(r.has(t), "{t} missing from inventory registry");
            let spec = r.get(t).unwrap();
            assert_eq!(spec.type_name, t);
        }
    }

    #[test]
    fn inventory_registry_returns_none_for_unknown() {
        assert!(InventorySpecRegistry.get("NoSuchBlock").is_none());
    }

    #[test]
    fn instantiate_blocks_builds_every_id() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{
                    "src":   {"type":"SineSource","params":{"rate_hz":250000}},
                    "decim": {"type":"Decimator","params":{"factor":5,"num_taps":41,"cutoff_normalized":0.08}},
                    "demod": {"type":"FmDemod","params":{"sample_rate_hz":50000,"max_deviation_hz":75000}}
                },
                "wires":[["src.out","decim.in"],["decim.out","demod.in"]]
            }"#,
        );
        let blocks = instantiate_blocks(&d).expect("all types registered");
        assert_eq!(blocks.len(), 3);
        for id in ["src", "decim", "demod"] {
            assert!(blocks.contains_key(id), "missing {id}");
        }
    }

    #[test]
    fn instantiate_blocks_defaults_missing_params() {
        // Neither block has `params` — each factory must fall back to its
        // Default impl via `BlockFactory::construct`.
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{
                    "src":   {"type":"SineSource"},
                    "decim": {"type":"Decimator"}
                },
                "wires":[["src.out","decim.in"]]
            }"#,
        );
        let blocks = instantiate_blocks(&d).expect("defaults cover every field");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn instantiate_blocks_rejects_unknown_type() {
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"x":{"type":"NotReal"}},
                "wires":[]
            }"#,
        );
        // `Box<dyn Block>` isn't Debug, so `.unwrap_err()` won't compile.
        let Err(err) = instantiate_blocks(&d) else {
            panic!("expected unknown-type error");
        };
        let msg = format!("{err}");
        assert!(msg.contains("NotReal"), "error should name the type: {msg}");
    }

    #[test]
    fn instantiate_blocks_surfaces_factory_errors() {
        // Decimator::new rejects factor=0 — the factory must forward that
        // error rather than panicking, and we wrap it with the block id.
        let d = doc_from(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"decim":{"type":"Decimator","params":{"factor":0,"num_taps":33,"cutoff_normalized":0.1}}},
                "wires":[]
            }"#,
        );
        let Err(err) = instantiate_blocks(&d) else {
            panic!("expected factor=0 to fail construction");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("decim") && msg.contains("Decimator"),
            "error should identify the failing block: {msg}"
        );
    }
}
