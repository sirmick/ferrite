//! Static registry of block types.
//!
//! Every `impl Block for T` annotated with [`#[ferrite_block]`][crate]
//! submits a [`BlockEntry`] here at link time via the [`inventory`]
//! crate. The registry is populated before `main` runs; enumeration is
//! allocation-free and iteration order is unspecified.
//!
//! The runtime uses this to advertise the set of block types it
//! supports without a hand-maintained table. Tests assert the expected
//! blocks are registered.

use anyhow::Result;

use crate::{Block, BlockSpec};

/// Function pointer that constructs a concrete block from a JSON params
/// value. Dispatches to `<T as BlockFactory>::construct` — every block
/// annotated with `#[ferrite_block]` must implement `BlockFactory`.
pub type ConstructFn = fn(&serde_json::Value) -> Result<Box<dyn Block>>;

/// One block type's registry entry. Holds function pointers rather than
/// inline values so the spec and the constructor stay single-sourced in
/// the block's own impl.
pub struct BlockEntry {
    pub spec_fn: fn() -> BlockSpec,
    pub construct_fn: ConstructFn,
}

impl BlockEntry {
    #[must_use]
    pub fn spec(&self) -> BlockSpec {
        (self.spec_fn)()
    }

    /// Construct an instance from a JSON params value. Pass
    /// `&serde_json::Value::Null` when the flowgraph omits the `params`
    /// object; the block's [`crate::BlockFactory`] impl defaults
    /// missing fields from its `Default`.
    pub fn construct(&self, params: &serde_json::Value) -> Result<Box<dyn Block>> {
        (self.construct_fn)(params)
    }
}

inventory::collect!(BlockEntry);

/// Iterate every registered block type. Order is link-time dependent.
pub fn entries() -> impl Iterator<Item = &'static BlockEntry> {
    inventory::iter::<BlockEntry>()
}

/// Find a registered block by its declared `type_name`.
#[must_use]
pub fn find(type_name: &str) -> Option<&'static BlockEntry> {
    entries().find(|e| e.spec().type_name == type_name)
}

#[cfg(test)]
mod tests {
    use super::find;

    #[test]
    fn construct_defaults_when_params_null() {
        // SineSource has a Default; a Null params value must produce an
        // instance using those defaults rather than failing.
        let entry = find("SineSource").expect("SineSource registered");
        entry
            .construct(&serde_json::Value::Null)
            .expect("defaults cover every field");
    }

    #[test]
    fn construct_merges_partial_params_over_defaults() {
        // Only specify one field; the rest come from Default via
        // #[serde(default)] at the container level.
        let entry = find("Decimator").expect("Decimator registered");
        let params = serde_json::json!({ "factor": 8 });
        entry.construct(&params).expect("valid Decimator params");
    }

    #[test]
    fn construct_rejects_bad_params() {
        // factor=0 is invalid — Decimator::new bails; the factory forwards
        // that error rather than panicking.
        let entry = find("Decimator").expect("Decimator registered");
        let params = serde_json::json!({ "factor": 0, "num_taps": 33, "cutoff_normalized": 0.1 });
        assert!(entry.construct(&params).is_err());
    }

    #[test]
    fn construct_rejects_unknown_fields() {
        // serde_json is lenient by default — unknown keys are ignored,
        // not rejected. This test pins that behaviour so a flowgraph can
        // carry extra comment-like fields without failing construction.
        let entry = find("SineSource").expect("SineSource registered");
        let params = serde_json::json!({ "rate_hz": 250_000.0, "_comment": "ignored" });
        entry.construct(&params).expect("extra fields tolerated");
    }

    /// Schema-rollout enforcement: every registered block must carry
    /// non-empty `ai_notes` (block-level + per-param). See
    /// `docs/13-ai-notes-migration.md` for the authoring guide. Length
    /// floor of 10 words on the block-level note rejects placeholder
    /// stubs without being so strict it discourages utility-block
    /// authors from one-line entries.
    #[test]
    fn every_registered_block_has_ai_notes() {
        let mut missing: Vec<String> = Vec::new();
        for entry in super::entries() {
            let s = entry.spec();
            if s.ai_notes.is_empty() {
                missing.push(format!("block `{}` ai_notes is empty", s.type_name));
            } else if s.ai_notes.split_whitespace().count() < 10 {
                missing.push(format!(
                    "block `{}` ai_notes is too terse (need ≥10 words)",
                    s.type_name
                ));
            }
            for p in s.params {
                if p.ai_notes.is_empty() {
                    missing.push(format!(
                        "param `{}::{}` ai_notes is empty",
                        s.type_name, p.key
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "ai_notes coverage incomplete:\n  - {}",
            missing.join("\n  - "),
        );
    }
}
