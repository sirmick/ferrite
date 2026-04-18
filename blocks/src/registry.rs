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

use crate::BlockSpec;

/// One block type's registry entry. Points at the block's static
/// [`BlockSpec`] factory rather than embedding the spec directly so the
/// spec stays single-sourced in the block's own impl.
pub struct BlockEntry {
    pub spec_fn: fn() -> BlockSpec,
}

impl BlockEntry {
    #[must_use]
    pub fn spec(&self) -> BlockSpec {
        (self.spec_fn)()
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
