//! End-to-end canary: `flowgraphs/dtmf-e2e.json` round-trips digits
//! across the cross-env bridge.
//!
//! The preset splits into a node half (DTMF → AM upmix → channelize →
//! decimate → `WsBridgeTx` on stream 1001) and a browser half
//! (`WsBridgeRx` → AM demod → DTMF decoder → `EventsSink`). This test
//! stands up both halves in-process: the node half runs on the real
//! tokio driver via [`spawn_preset`] (the same code path `ferrited`
//! uses), its `BroadcastSink` fan-out is subscribed here, and every
//! `IqF32` frame on the cross-env stream is converted back to floats
//! and pushed into the browser-half runtime's `WsBridgeRx`. The
//! browser half ticks inline and we drain its `EventsSink` until we
//! see `["1", "2", "3", "4"]`.
//!
//! This proves the whole cross-env contract — bridges, scheduler rate
//! semantics across the 2.4 MS/s → 8 kS/s fan-down, serde of the
//! postcard `Frame` wire format, and the decoder's events path —
//! without needing a subprocess, a browser, or the WASM runtime. The
//! sibling native test [`runtime::dtmf_e2e`] covers the same events
//! chain at 8 kS/s single-env; this one adds the IQ round-trip the
//! preset exists for.

use std::time::Duration;

use ferrite_blocks::{frame::Frame, EventsSink, WsBridgeRx};
use ferrite_runtime::{
    split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry, Runtime,
    CROSS_ENV_STREAM_BASE, DEFAULT_FRAMES_HINT, DEFAULT_TICK_PERIOD,
};

#[path = "../src/log_stream.rs"]
#[allow(dead_code)]
mod log_stream;

#[path = "../src/frame_bus.rs"]
#[allow(dead_code)]
mod frame_bus;

#[path = "../src/bridge_sink.rs"]
#[allow(dead_code)]
mod bridge_sink;

#[path = "../src/decoder_store.rs"]
#[allow(dead_code)]
mod decoder_store;

#[path = "../src/preset_pipeline.rs"]
#[allow(dead_code)]
mod preset_pipeline;

// `app_state` + `routes` reference `crate::view_bridge::…`; this
// #[path] re-include has to mirror every src module they touch or the
// test binary won't resolve them. view_bridge.rs is self-contained
// (std + serde + tokio, no crate:: deps), so the shim is a plain mod.
#[path = "../src/view_bridge.rs"]
#[allow(dead_code)]
mod view_bridge;

#[path = "../src/band_plan.rs"]
#[allow(dead_code)]
mod band_plan;

#[path = "../src/source_policy.rs"]
#[allow(dead_code)]
mod source_policy;

#[path = "../src/app_state.rs"]
#[allow(dead_code)]
mod app_state;

#[path = "../src/device.rs"]
#[allow(dead_code)]
mod device;

#[path = "../src/device_cache.rs"]
#[allow(dead_code)]
mod device_cache;

#[path = "../src/block_schema.rs"]
#[allow(dead_code)]
mod block_schema;

#[path = "../src/routes.rs"]
#[allow(dead_code, clippy::unused_async)]
mod routes;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtmf_e2e_preset_round_trips_digits_across_cross_env_bridge() {
    let preset_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("flowgraphs/dtmf-e2e.json");
    let doc: FlowgraphDoc = serde_json::from_str(
        &std::fs::read_to_string(&preset_path).expect("dtmf-e2e.json readable"),
    )
    .expect("dtmf-e2e.json parses");

    // --- Browser half: in-process runtime -----------------------------
    // The scheduler is sync; we own the tick cadence in this test loop.
    let browser_half = split_for_environment(&doc, Environment::Browser, &InventorySpecRegistry)
        .expect("browser split");
    let mut browser_rt = Runtime::load_doc(
        &browser_half,
        Environment::Browser,
        DEFAULT_FRAMES_HINT,
        DEFAULT_TICK_PERIOD,
    )
    .expect("browser runtime loads");
    browser_rt.init().expect("browser init");
    browser_rt.start().expect("browser start");

    // Stream id + block id are deterministic from `env_split`'s wire-
    // order allocation. Slot 0 is `ui:fft` (FftU8), slot 1 is the
    // cross-env IQ wire, so CROSS_ENV_STREAM_BASE+1 is where the
    // DTMF signal arrives.
    let cross_stream_id = u16::try_from(CROSS_ENV_STREAM_BASE + 1)
        .expect("CROSS_ENV_STREAM_BASE+1 fits u16 — Frame::IqF32.stream_id is u16");
    let cross_rx_id = format!("__bridge_rx_{}", CROSS_ENV_STREAM_BASE + 1);

    // --- Node half: spawn_preset with a FrameBus we own ---------------
    // A 5 ms tick period is ferrited's default; at DEFAULT_FRAMES_HINT
    // samples per tick that's a healthy drive rate for a 2.4 MS/s
    // source-side pipeline running as fast as the CPU will oblige.
    // Subscriber capacity sized to hold a generous slack of in-flight
    // frames so this test's sub never drops under its own pump latency.
    let frames = frame_bus::FrameBus::new();
    let mut rx = frames.subscribe(1024);
    let mount = preset_pipeline::spawn_preset(
        &doc,
        frames,
        Duration::from_millis(5),
        std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        std::sync::Arc::new(decoder_store::DecoderStore::new()),
    )
    .expect("node half spawns");

    // --- Pump loop ----------------------------------------------------
    // Alternate between:
    //   1. Draining any frames queued on our per-sub mpsc receiver,
    //      routing stream-1001 IQ into the browser's WsBridgeRx.
    //   2. Ticking the browser runtime so buffered samples flow.
    //   3. Draining the EventsSink and recording any decoded digits.
    // Bail early once we've collected 4 digits; otherwise run until the
    // deadline and fail with a descriptive diagnostic.
    let mut digits: Vec<String> = Vec::new();
    let mut ticks_without_data = 0_u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline && digits.len() < 4 {
        // Short recv timeout keeps the browser tick loop responsive
        // even when the node half is momentarily quiet.
        let mut saw_any = false;
        match tokio::time::timeout(Duration::from_millis(5), rx.recv()).await {
            Ok(Some(bytes)) => {
                saw_any = true;
                if let Ok(Frame::IqF32 {
                    stream_id, payload, ..
                }) = Frame::from_postcard(&bytes)
                {
                    if stream_id == cross_stream_id {
                        push_iq_payload(&mut browser_rt, &cross_rx_id, &payload);
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }

        browser_rt.tick().expect("browser tick");

        let events_sink = browser_rt
            .block_typed::<EventsSink>("events")
            .expect("EventsSink block reachable by id");
        let mut saw_events = false;
        for bytes in events_sink.drain_events() {
            saw_events = true;
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!("event not JSON: {e}: {:?}", String::from_utf8_lossy(&bytes))
            });
            digits.push(
                v["digit"]
                    .as_str()
                    .expect("event has `digit` field")
                    .to_string(),
            );
        }

        if saw_any || saw_events {
            ticks_without_data = 0;
        } else {
            ticks_without_data = ticks_without_data.saturating_add(1);
            // If absolutely nothing flows for a long stretch, yield so
            // the spawn_preset task gets CPU time on the single-worker
            // tokio runtime.
            if ticks_without_data.is_multiple_of(64) {
                tokio::task::yield_now().await;
            }
        }
    }

    // Graceful teardown first so a failed assertion doesn't leak the
    // spawned runtime task across tests.
    mount
        .handle
        .shutdown()
        .await
        .expect("node half shuts down cleanly");
    let _ = browser_rt.stop();

    assert_eq!(
        digits,
        vec!["1".to_string(), "2".into(), "3".into(), "4".into()],
        "expected [1,2,3,4] across the cross-env bridge; got {digits:?}",
    );
}

/// Decode a postcard `IqF32` payload (LE interleaved f32s) into floats
/// and push them into the named `WsBridgeRx`. Mirrors what the browser
/// runner does in production, minus the WASM boundary.
fn push_iq_payload(rt: &mut Runtime, block_id: &str, payload: &[u8]) {
    // Payload is `[re0, im0, re1, im1, …]` as LE bytes. push_interleaved
    // takes a `&[f32]` and pairs them up internally.
    let n = payload.len() / 4;
    let mut floats = Vec::with_capacity(n);
    for chunk in payload.chunks_exact(4) {
        floats.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let rx_block = rt
        .block_typed::<WsBridgeRx>(block_id)
        .unwrap_or_else(|| panic!("browser runtime missing {block_id:?}"));
    rx_block.push_interleaved(&floats);
}
