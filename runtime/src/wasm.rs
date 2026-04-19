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

use crate::block_registry::InventorySpecRegistry;
use crate::doc::{Environment, FlowgraphDoc};
use crate::env_split::split_for_environment;
use crate::validate::validate_doc;

/// Crate version, exposed so the browser can log "runtime vX.Y.Z loaded"
/// and any protocol-level version checks have something to key off of.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Parse a flowgraph JSON string and run the registry-independent
/// validation passes (shape / fan / dag / wire_endpoints). Returns the
/// doc's `name` on success; throws a `JsError` carrying the validation
/// summary otherwise.
///
/// Registry-dependent phases (env_match, wire_type_match, params) still
/// require a block registry; a later commit exposes that path once the
/// wasm-side registry shape is designed.
#[wasm_bindgen(js_name = parseAndValidateDoc)]
pub fn parse_and_validate_doc(json: &str) -> Result<String, JsError> {
    let doc: FlowgraphDoc = serde_json::from_str(json)
        .map_err(|e| JsError::new(&format!("flowgraph JSON parse error: {e}")))?;
    validate_doc(&doc).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(doc.name)
}

/// Split a flowgraph JSON doc for the named environment (`"node"` or
/// `"browser"`). Returns the resulting subgraph as a JSON string, with
/// `WsBridgeTx`/`WsBridgeRx` pairs auto-inserted for every cross-env
/// wire. Throws a `JsError` on invalid env string, parse failure, or
/// split failure (bad placement, unresolved Either block, etc.).
///
/// The inventory registry is linked in automatically — every block type
/// compiled into this crate is available for placement resolution.
#[wasm_bindgen(js_name = splitDocForEnvironment)]
pub fn split_doc_for_environment(json: &str, env: &str) -> Result<String, JsError> {
    let target = match env {
        "node" => Environment::Node,
        "browser" => Environment::Browser,
        other => return Err(JsError::new(&format!("unknown environment {other:?}"))),
    };
    let doc: FlowgraphDoc = serde_json::from_str(json)
        .map_err(|e| JsError::new(&format!("flowgraph JSON parse error: {e}")))?;
    let registry = InventorySpecRegistry;
    let out =
        split_for_environment(&doc, target, &registry).map_err(|e| JsError::new(&e.to_string()))?;
    serde_json::to_string(&out)
        .map_err(|e| JsError::new(&format!("split result serialization failed: {e}")))
}
