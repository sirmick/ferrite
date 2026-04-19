//! wasm-bindgen facade.
//!
//! The browser-side flowgraph runner imports this crate as an ES module
//! via wasm-pack. Everything exported from here is the surface the TS
//! runner code reaches through — doc parsing, block registration, tick
//! pump, lifecycle. Kept deliberately thin; the actual logic lives in
//! the env-agnostic modules and is shared with the native build.
//!
//! This module is gated on `feature = "wasm"`; `wasm-pack build` enables
//! it, native builds compile without any wasm-bindgen dependency pulled
//! in.

use wasm_bindgen::prelude::*;

/// Crate version, exposed so the browser can log "runtime vX.Y.Z loaded"
/// and any protocol-level version checks have something to key off of.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
