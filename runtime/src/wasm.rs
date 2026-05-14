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

use ferrite_blocks::{AudioSink, EventsSink, WsBridgeRx, WsBridgeRxF32};
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

use crate::block_registry::InventorySpecRegistry;
use crate::doc::{Environment, FlowgraphDoc};
use crate::env_split::split_for_environment;
use crate::reconfigure::ReconfigurePlan;
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
    /// use [`DEFAULT_FRAMES_HINT`]. `tick_period_us` overrides the
    /// assumed scheduler cadence used for source-ring sizing; `None`
    /// uses [`DEFAULT_TICK_PERIOD`] (400µs).
    #[wasm_bindgen(constructor)]
    pub fn new(
        doc_json: &str,
        env: &str,
        frames_hint: Option<usize>,
        tick_period_us: Option<u32>,
    ) -> Result<RuntimeHandle, JsError> {
        let target = parse_environment(env)?;
        let doc = parse_doc(doc_json)?;
        let tick_period = tick_period_us
            .map(|us| std::time::Duration::from_micros(u64::from(us)))
            .unwrap_or(crate::runtime::DEFAULT_TICK_PERIOD);
        let rt = Runtime::load_doc(
            &doc,
            target,
            frames_hint.unwrap_or(DEFAULT_FRAMES_HINT),
            tick_period,
        )
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

    /// Push a batch of real `f32` audio samples into the named
    /// `WsBridgeRxF32` block. Real-audio analog of [`pushIq`]: the
    /// browser runner forwards each decoded `Frame::F32` payload here,
    /// the block buffers internally and emits on its `out` port at
    /// tick time. Errors if the block doesn't exist or isn't a
    /// `WsBridgeRxF32`.
    #[wasm_bindgen(js_name = pushF32)]
    pub fn push_f32(&mut self, block_id: &str, samples: &[f32]) -> Result<(), JsError> {
        let src = self
            .rt
            .block_typed::<WsBridgeRxF32>(block_id)
            .ok_or_else(|| JsError::new(&format!("no WsBridgeRxF32 block named {block_id:?}")))?;
        src.push(samples);
        Ok(())
    }

    /// Record the sample rate advertised by an arriving frame on the
    /// named bridge-Rx block (either `WsBridgeRx` for IQ or
    /// `WsBridgeRxF32` for real audio). The browser runner reads
    /// `frame.sample_rate_hz` and calls this whenever it's non-zero so
    /// dynamic source-rate changes cross the WS boundary. Cheap;
    /// actual propagation to downstream blocks happens on the next
    /// `refreshRates()` sweep.
    #[wasm_bindgen(js_name = setBridgeRxRate)]
    pub fn set_bridge_rx_rate(
        &mut self,
        block_id: &str,
        sample_rate_hz: f64,
    ) -> Result<(), JsError> {
        if let Some(src) = self.rt.block_typed::<WsBridgeRx>(block_id) {
            src.set_advertised_rate(sample_rate_hz);
            return Ok(());
        }
        if let Some(src) = self.rt.block_typed::<WsBridgeRxF32>(block_id) {
            src.set_advertised_rate(sample_rate_hz);
            return Ok(());
        }
        Err(JsError::new(&format!(
            "no WsBridgeRx/WsBridgeRxF32 block named {block_id:?}"
        )))
    }

    /// Re-run the rate-propagation pass across the graph. Intended to
    /// be called once per second from the tick loop so `WsBridgeRx`'s
    /// dynamically observed rate (updated via `setBridgeRxRate`)
    /// re-propagates to downstream resamplers / demods.
    #[wasm_bindgen(js_name = refreshRates)]
    pub fn refresh_rates(&mut self) -> Result<(), JsError> {
        self.rt
            .refresh_rates()
            .map_err(|e| JsError::new(&format!("{e:#}")))
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

    /// Cumulative per-block flow snapshot as a JSON string. Shape is
    /// [`ferrite_runtime::DiagSnapshot`] serialized via serde — the JS
    /// side parses it and emits deltas against its last cached copy.
    /// Returning a string keeps the wasm_bindgen surface tiny (one owned
    /// `String` vs a ~40-field JsValue tree); parse cost is ~microseconds
    /// for a 10-block preset at 1 Hz.
    #[wasm_bindgen(js_name = diagSnapshot)]
    pub fn diag_snapshot(&self) -> Result<String, JsError> {
        serde_json::to_string(&self.rt.diag_snapshot())
            .map_err(|e| JsError::new(&format!("diag snapshot serialize: {e}")))
    }

    /// Drain every buffered event from the named `EventsSink` as an
    /// array of UTF-8 JSON strings (one event per entry, no trailing
    /// newline). Mirrors [`Self::drain_audio`] — the JS host calls this
    /// after each tick and forwards the events to whatever consumer the
    /// preset defines (vitest harness, `postMessage` to the page, …).
    /// Errors if no block with that id exists, or if it exists but
    /// isn't an `EventsSink`.
    #[wasm_bindgen(js_name = drainEvents)]
    pub fn drain_events(&mut self, block_id: &str) -> Result<Vec<JsValue>, JsError> {
        let sink = self
            .rt
            .block_typed::<EventsSink>(block_id)
            .ok_or_else(|| JsError::new(&format!("no EventsSink block named {block_id:?}")))?;
        let raw = sink.drain_events();
        let mut out = Vec::with_capacity(raw.len());
        for bytes in raw {
            let s = String::from_utf8(bytes)
                .map_err(|e| JsError::new(&format!("EventsSink emitted non-UTF8 bytes: {e}")))?;
            out.push(JsValue::from_str(&s));
        }
        Ok(out)
    }

    /// Cumulative count of events the named `EventsSink` has dropped
    /// because its buffer was full. Surfaced to tests/UIs as a health
    /// signal.
    #[wasm_bindgen(js_name = eventsDropped)]
    pub fn events_dropped(&mut self, block_id: &str) -> Result<u64, JsError> {
        let sink = self
            .rt
            .block_typed::<EventsSink>(block_id)
            .ok_or_else(|| JsError::new(&format!("no EventsSink block named {block_id:?}")))?;
        Ok(sink.dropped())
    }

    /// Apply a params delta to one browser-side block and reconfigure
    /// in place. Tries the block's `apply_live_params` hot path first
    /// (phase-continuous for blocks that opt in), falls back to a
    /// block-scoped rebuild on Ok(false). Same decision the server's
    /// `apply_block_params` makes for node-side blocks — keeps the two
    /// halves consistent.
    ///
    /// `delta_json` is a JSON-encoded object (`{"key":value…}`); keys
    /// present replace, keys absent stay. Returns a JSON string with
    /// the same wire shape as the REST `POST /api/pipeline/blocks/:id/params`
    /// response so the web dispatcher can treat REST and WASM replies
    /// with a single type.
    ///
    /// The runtime's rollback contract carries through — if the merged
    /// doc fails to build, the browser runtime is left untouched.
    #[wasm_bindgen(js_name = reconfigureBlock)]
    pub fn reconfigure_block(&mut self, id: &str, delta_json: &str) -> Result<String, JsError> {
        let delta: Value = serde_json::from_str(delta_json)
            .map_err(|e| JsError::new(&format!("delta JSON parse error: {e}")))?;
        let plan = self
            .rt
            .live_reconfigure_block(id, delta)
            .map_err(|e| JsError::new(&format!("{e:#}")))?;
        let body = reconfigure_response_json(&plan);
        serde_json::to_string(&body)
            .map_err(|e| JsError::new(&format!("reconfigure response serialize: {e}")))
    }
}

/// Build the JSON body emitted by `reconfigureBlock`. Shape matches
/// `ReconfigureResponse` in `server/src/routes.rs` so the TS dispatcher
/// can treat REST and WASM replies with a single type.
fn reconfigure_response_json(plan: &ReconfigurePlan) -> Value {
    json!({
        "applied": true,
        "overall": plan.overall.as_wire_str(),
        "changes": plan.changes.iter().map(|c| json!({
            "block_id": c.block_id,
            "param_key": c.param_key,
            "old_value": c.old_value,
            "new_value": c.new_value,
            "scope": c.scope.as_wire_str(),
        })).collect::<Vec<_>>(),
        "structural_count": plan.structural.len(),
        "noop": plan.is_noop(),
    })
}
