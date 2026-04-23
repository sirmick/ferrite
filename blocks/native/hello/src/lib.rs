//! M1 substrate proof — calls a vendored C library from Rust.
//!
//! This crate exists to validate the `blocks/native/` build pipeline
//! end-to-end on both targets:
//!
//! - **native** (`cargo test`): cc + system gcc compiles `csrc/hello.c`
//!   into a static archive that the host linker pulls in.
//! - **wasm32-unknown-unknown** (`wasm-pack build`): cc + clang with
//!   `--target=wasm32-unknown-unknown -nostdlib` produces a WASM
//!   object that wasm-bindgen + wasm-ld assemble into the final blob.
//!
//! Once the assertions in this crate's tests pass on both targets, the
//! pattern is proven and real C vendors (liquid-dsp, multimon-ng) get
//! built using the same `build.rs` template plus wasi-libc on top.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use core::ffi::c_int;

extern "C" {
    fn hello_add(a: c_int, b: c_int) -> c_int;
    fn hello_sumf(samples: *const f32, n: c_int) -> f32;
    fn hello_rmsf(samples: *const f32, n: c_int) -> f32;
}

/// Safe Rust wrapper around the C `hello_add` function.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn add(a: i32, b: i32) -> i32 {
    // SAFETY: `hello_add` is pure — no allocation, no globals, no
    // concurrency hazards. Trivially safe to call from any context.
    unsafe { hello_add(a, b) }
}

/// Safe Rust wrapper around the C `hello_sumf` function. The slice
/// length is converted to the C `int` ABI; rejects oversized slices
/// rather than truncating silently.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn sumf(samples: &[f32]) -> f32 {
    // SAFETY: pointer + length come from a live slice; the C side reads
    // exactly `n` floats and never writes. `i32::try_from` rejects
    // anything that would overflow the int ABI.
    let n = i32::try_from(samples.len()).expect("hello_sumf: slice length exceeds c_int range");
    if n == 0 {
        return 0.0;
    }
    unsafe { hello_sumf(samples.as_ptr(), n) }
}

/// RMS of `samples`. Exists to drag in libm (`sqrtf`) — proves the
/// libm link path works for both targets, which is the gate before
/// vendoring liquid-dsp.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn rmsf(samples: &[f32]) -> f32 {
    let n = i32::try_from(samples.len()).expect("hello_rmsf: slice length exceeds c_int range");
    if n == 0 {
        return 0.0;
    }
    // SAFETY: pointer + length come from a live slice; C reads only.
    unsafe { hello_rmsf(samples.as_ptr(), n) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_proves_int_abi() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-1, 1), 0);
        assert_eq!(add(i32::MAX, 0), i32::MAX);
    }

    #[test]
    fn rmsf_proves_libm_link() {
        // RMS of [1, -1, 1, -1, …] = 1.0; uses sqrtf so it pulls in libm.
        let xs = [1.0_f32, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        let r = rmsf(&xs);
        assert!((r - 1.0).abs() < 1e-6, "rmsf = {r}, expected 1.0");
        // Empty slice short-circuits, doesn't divide by zero.
        assert_eq!(rmsf(&[]), 0.0);
    }

    #[test]
    fn sumf_proves_pointer_length_abi() {
        assert_eq!(sumf(&[]), 0.0);
        assert_eq!(sumf(&[1.0, 2.0, 3.0]), 6.0);
        // A larger slice catches off-by-one in the loop and aliasing
        // bugs in the C side.
        let xs: Vec<f32> = (0..1024).map(|i| i as f32).collect();
        let expected: f32 = xs.iter().sum();
        let got = sumf(&xs);
        assert!(
            (got - expected).abs() < 1e-3,
            "sumf({}) = {got} expected {expected}",
            xs.len()
        );
    }
}
