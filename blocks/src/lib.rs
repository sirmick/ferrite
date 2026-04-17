//! Ferrite DSP blocks — skeleton.
//!
//! The Block trait, port types, param schemas, and initial blocks land
//! in Phase B (see `docs/10-commits.md`). For now this crate exists so
//! the workspace compiles from the first commit and so the WASM build
//! path is exercised end-to-end by a trivial demo block.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Trivial placeholder block — proves the Rust→WASM→Worker path.
/// Replaced by real DSP blocks in Phase B.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn demo_add(a: f32, b: f32) -> f32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::{demo_add, version};

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn demo_add_sums() {
        assert!((demo_add(1.5, 2.25) - 3.75).abs() < f32::EPSILON);
    }
}
