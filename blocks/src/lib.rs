//! Ferrite DSP blocks — dual-compile crate (native + WebAssembly).
//!
//! The [`block`] module defines the trait and static descriptors every
//! block implements; concrete blocks (`SineSource`, `FFT`, …) land in
//! sibling modules as Phase B progresses.
//!
//! Each block impl is annotated with [`ferrite_block`] so it
//! self-registers into [`registry`] at link time.

// Lets the `#[ferrite_block]` macro emit `::ferrite_blocks::…` paths
// that resolve both from downstream crates and from inside this crate.
extern crate self as ferrite_blocks;

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

pub mod block;
pub mod channelizer;
pub mod decimator;
pub mod fft;
pub mod file_source;
pub mod log_mag_u8;
pub mod registry;
pub mod sine;

pub use block::{
    Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, ParamKind, ParamSpec,
    Placement, PortMeta, PortSpec, PortType, Work, MAX_PORTS,
};
pub use channelizer::{Channelizer, ChannelizerParams};
pub use decimator::{Decimator, DecimatorParams};
pub use fft::{FftBlock, FftBlockParams, FftWindow};
pub use file_source::{FileIqSource, FileIqSourceParams, IqFileFormat, ReadSeek};
pub use log_mag_u8::{LogMagU8, LogMagU8Params};
pub use sine::{SineSource, SineSourceParams};

/// Marks an `impl Block for T` so `T` is added to [`registry`].
///
/// Re-exported from `ferrite-blocks-macros` for ergonomic use; the
/// macro's generated code references `::ferrite_blocks::…` paths.
pub use ferrite_blocks_macros::ferrite_block;

/// Re-exported so the generated code from [`ferrite_block`] can refer to
/// [`inventory`] without requiring callers to add a direct dep.
#[doc(hidden)]
pub use inventory;

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Trivial placeholder — proves the Rust→WASM→Worker path. Replaced by
/// real DSP blocks as Phase B progresses.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn demo_add(a: f32, b: f32) -> f32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::{demo_add, registry, version};
    use std::collections::HashSet;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn demo_add_sums() {
        assert!((demo_add(1.5, 2.25) - 3.75).abs() < f32::EPSILON);
    }

    #[test]
    fn registry_contains_every_shipped_block() {
        let names: HashSet<&'static str> =
            registry::entries().map(|e| e.spec().type_name).collect();
        for expected in [
            "SineSource",
            "FFT",
            "FileIqSource",
            "LogMagU8",
            "Decimator",
            "Channelizer",
        ] {
            assert!(
                names.contains(expected),
                "{expected} missing from registry (found: {names:?})",
            );
        }
    }

    #[test]
    fn registry_find_returns_matching_entry() {
        let entry = registry::find("SineSource").expect("SineSource must be registered");
        assert_eq!(entry.spec().type_name, "SineSource");
    }

    #[test]
    fn registry_find_rejects_unknown_names() {
        assert!(registry::find("NoSuchBlock").is_none());
    }
}
