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

use ferrite_blocks::{AudioSink, WsBridgeRx};
use wasm_bindgen::prelude::*;

use crate::block_registry::InventorySpecRegistry;
use crate::doc::{Environment, FlowgraphDoc};
use crate::env_split::split_for_environment;
use crate::runtime::{Runtime, DEFAULT_FRAMES_HINT};
use crate::validate::validate_doc;

fn parse_environment(env: &str) -> Result<Environment, JsError> {
    match env {
        "node" => Ok(Environment::Node),
        "browser" => Ok(Environment::Browser),
        other => Err(JsError::new(&format!("unknown environment {other:?}"))),
    }
}

fn parse_doc(json: &str) -> Result<FlowgraphDoc, JsError> {
    serde_json::from_str(json)
        .map_err(|e| JsError::new(&format!("flowgraph JSON parse error: {e}")))
}

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
    let doc = parse_doc(json)?;
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
    let target = parse_environment(env)?;
    let doc = parse_doc(json)?;
    let registry = InventorySpecRegistry;
    let out =
        split_for_environment(&doc, target, &registry).map_err(|e| JsError::new(&e.to_string()))?;
    serde_json::to_string(&out)
        .map_err(|e| JsError::new(&format!("split result serialization failed: {e}")))
}

/// Browser-side handle over a [`Runtime`]. Constructing it parses, validates,
/// instantiates, and assembles the graph — the result is a fully-wired tick
/// pump sitting in `Created` state. From there the JS runner drives
/// `init` → `tick*` → `stop` explicitly; the cadence lives on the JS side.
///
/// The wrapper is a thin shim over [`Runtime`]'s methods: each call
/// forwards directly, converting `anyhow::Error` into a `JsError` the JS
/// wrapper can surface as a thrown `Error`.
#[wasm_bindgen(js_name = RuntimeHandle)]
pub struct RuntimeHandle {
    rt: Runtime,
}

#[wasm_bindgen(js_class = RuntimeHandle)]
impl RuntimeHandle {
    /// Build a runtime from a flowgraph JSON doc targeting the given
    /// environment (typically `"browser"`). `frames_hint` overrides the
    /// per-call frame budget; pass `None` (or `undefined` from JS) to
    /// use [`DEFAULT_FRAMES_HINT`].
    #[wasm_bindgen(constructor)]
    pub fn new(
        doc_json: &str,
        env: &str,
        frames_hint: Option<usize>,
    ) -> Result<RuntimeHandle, JsError> {
        let target = parse_environment(env)?;
        let doc = parse_doc(doc_json)?;
        let rt = Runtime::load_doc(&doc, target, frames_hint.unwrap_or(DEFAULT_FRAMES_HINT))
            .map_err(|e| JsError::new(&format!("{e:#}")))?;
        Ok(Self { rt })
    }

    /// Call `init` on every block. Required before the first `tick`.
    pub fn init(&mut self) -> Result<(), JsError> {
        self.rt.init().map_err(|e| JsError::new(&format!("{e:#}")))
    }

    /// Transition `Initialized → Running`. Optional — `tick` works from
    /// `Initialized` too; calling `start` is only needed if a later
    /// feature keys off the `Running` state.
    pub fn start(&mut self) -> Result<(), JsError> {
        self.rt.start().map_err(|e| JsError::new(&format!("{e:#}")))
    }

    /// Run one tick of the graph. Legal from `Initialized` or `Running`.
    pub fn tick(&mut self) -> Result<(), JsError> {
        self.rt.tick().map_err(|e| JsError::new(&format!("{e:#}")))
    }

    /// Drain every block in reverse topological order. Terminal — the
    /// handle cannot be re-used afterwards; the JS side should drop its
    /// reference.
    pub fn stop(&mut self) -> Result<(), JsError> {
        self.rt.stop().map_err(|e| JsError::new(&format!("{e:#}")))
    }

    /// Current lifecycle state as a string: `"Created"`, `"Initialized"`,
    /// `"Running"`, or `"Stopped"`. Handy for tests and dev-console
    /// inspection; the JS runner usually tracks state itself.
    #[wasm_bindgen(getter)]
    pub fn state(&self) -> String {
        format!("{:?}", self.rt.state())
    }

    /// Per-call frame budget this runtime was built with.
    #[wasm_bindgen(getter, js_name = framesHint)]
    pub fn frames_hint(&self) -> usize {
        self.rt.frames_hint()
    }

    /// Drain accumulated samples from the named `AudioSink` block into
    /// `out`. Returns the number of samples actually copied — shorter
    /// than `out.len()` means the ring drained dry.
    ///
    /// The browser runner calls this after each `tick` and pushes the
    /// samples into a `SharedArrayBuffer` that the `AudioWorklet`
    /// drains on the audio clock. Errors if no block with that id
    /// exists, or if it exists but isn't an `AudioSink`.
    #[wasm_bindgen(js_name = drainAudio)]
    pub fn drain_audio(&mut self, block_id: &str, out: &mut [f32]) -> Result<usize, JsError> {
        let sink = self
            .rt
            .block_typed::<AudioSink>(block_id)
            .ok_or_else(|| JsError::new(&format!("no AudioSink block named {block_id:?}")))?;
        Ok(sink.ring_mut().read(out))
    }

    /// Cumulative count of samples the named `AudioSink` has dropped
    /// because its ring was full. Browser UIs surface this as a glitch
    /// counter.
    #[wasm_bindgen(js_name = audioDroppedSamples)]
    pub fn audio_dropped_samples(&mut self, block_id: &str) -> Result<u64, JsError> {
        let sink = self
            .rt
            .block_typed::<AudioSink>(block_id)
            .ok_or_else(|| JsError::new(&format!("no AudioSink block named {block_id:?}")))?;
        Ok(sink.dropped_samples())
    }

    /// Push a batch of interleaved `[i0, q0, i1, q1, …]` IQ floats into
    /// the named `WsBridgeRx` block. The browser runner owns the
    /// multiplexed WebSocket and forwards incoming frames through here;
    /// the block holds an internal ring and emits the samples on its
    /// `out` port at tick time. Odd-length buffers drop the trailing
    /// half-sample. Errors if the block doesn't exist or isn't a
    /// `WsBridgeRx`.
    #[wasm_bindgen(js_name = pushIq)]
    pub fn push_iq(&mut self, block_id: &str, samples: &[f32]) -> Result<(), JsError> {
        let src = self
            .rt
            .block_typed::<WsBridgeRx>(block_id)
            .ok_or_else(|| JsError::new(&format!("no WsBridgeRx block named {block_id:?}")))?;
        src.push_interleaved(samples);
        Ok(())
    }

    /// Complex samples currently buffered inside the named `WsBridgeRx`
    /// — written by `pushIq` but not yet emitted on the output port.
    /// Useful for test assertions and for surfacing queue depth to the
    /// UI.
    #[wasm_bindgen(js_name = iqBufferedSamples)]
    pub fn iq_buffered_samples(&mut self, block_id: &str) -> Result<usize, JsError> {
        let src = self
            .rt
            .block_typed::<WsBridgeRx>(block_id)
            .ok_or_else(|| JsError::new(&format!("no WsBridgeRx block named {block_id:?}")))?;
        Ok(src.buffered_samples())
    }

    /// Cumulative count of complex samples the named `WsBridgeRx` has
    /// dropped because its ring was full. UI surfaces this as a
    /// packet-loss indicator for the WS transport.
    #[wasm_bindgen(js_name = iqDroppedSamples)]
    pub fn iq_dropped_samples(&mut self, block_id: &str) -> Result<u64, JsError> {
        let src = self
            .rt
            .block_typed::<WsBridgeRx>(block_id)
            .ok_or_else(|| JsError::new(&format!("no WsBridgeRx block named {block_id:?}")))?;
        Ok(src.dropped_samples())
    }
}
