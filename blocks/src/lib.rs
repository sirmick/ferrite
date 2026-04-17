//! Ferrite DSP blocks — skeleton.
//!
//! The Block trait, port types, param schemas, and initial blocks land in
//! Phase B (see `docs/10-commits.md`). This file exists so the workspace
//! compiles from the first commit.

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }
}
