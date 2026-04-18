//! Ferrite DSP blocks — dual-compile crate (native + WebAssembly).
//!
//! The [`block`] module defines the trait and static descriptors every
//! block implements; concrete blocks (`SineSource`, `FFT`, …) land in
//! sibling modules as Phase B progresses.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

pub mod block;
pub mod decimator;
pub mod fft;
pub mod file_source;
pub mod log_mag_u8;
pub mod sine;

pub use block::{
    Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, ParamKind, ParamSpec,
    Placement, PortMeta, PortSpec, PortType, Work, MAX_PORTS,
};
pub use decimator::{Decimator, DecimatorParams};
pub use fft::{FftBlock, FftBlockParams, FftWindow};
pub use file_source::{FileIqSource, FileIqSourceParams, IqFileFormat, ReadSeek};
pub use log_mag_u8::{LogMagU8, LogMagU8Params};
pub use sine::{SineSource, SineSourceParams};

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
