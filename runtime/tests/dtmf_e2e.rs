//! Pre-Phase-1 DTMF events-bridge canary — native runtime half.
//!
//! Two tests:
//!
//! 1. [`dtmf_chain_runs_end_to_end_and_decodes_digits`] loads a
//!    stripped-down single-env flowgraph (`DtmfAudioSource →
//!    DtmfDecoder → EventsSink`) through the real `Runtime`, ticks it
//!    to completion, reaches into the instantiated `EventsSink` with
//!    `block_typed`, and asserts the digit stream parses as `["1",
//!    "2", "3", "4"]`. This is the integration-level proof that the
//!    runtime scheduler, typed port buffers, and the `Events` byte
//!    path all agree end-to-end.
//! 2. [`dtmf_e2e_preset_splits_into_both_halves_cleanly`] loads the
//!    shipped `flowgraphs/dtmf-e2e.json` and runs `env_split` for
//!    both environments. It asserts `env_split`'s outputs have the
//!    right shape — bridge pairs on the crossing, `ui:fft` tap on the
//!    producer half only, stream ids allocated from
//!    `CROSS_ENV_STREAM_BASE`. The cross-env AM/Channelizer chain is
//!    exercised on the wire by the sibling web vitest harness that
//!    spawns ferrited and drives the browser half over a real
//!    WebSocket.
//!
//! The audio/IQ rate-conversion round-trip (AM upmix → channelize →
//! decimate → AM demod) is deliberately left out of test 1. Today's
//! runtime allocates per-tick output buffers sized by `frames_hint`,
//! not by producer/consumer rate ratios — a 300× upsample like the
//! preset's AM modulator would drop most of its input every tick. The
//! preset's full chain still runs when each half is hosted in its
//! native environment (server + browser), because the WS transport
//! sits between producer ticks and consumer pulls; the canary that
//! exercises that is the vitest E2E, not this one.

use ferrite_blocks::EventsSink;
use ferrite_runtime::{
    split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry, Runtime,
    CROSS_ENV_STREAM_BASE, DEFAULT_FRAMES_HINT, DEFAULT_TICK_PERIOD,
};
use serde_json::{json, Value};

#[test]
fn dtmf_chain_runs_end_to_end_and_decodes_digits() {
    // Doc matches the events-path slice of `dtmf-e2e.json` but holds
    // everything at the decoder's 8 kS/s rate — no AM round-trip — so
    // the runtime's per-tick budget doesn't have to cope with a 300×
    // upsample.
    let doc: FlowgraphDoc = serde_json::from_value(json!({
        "name": "dtmf-events-native",
        "environments": ["node"],
        "blocks": {
            "src": {
                "type": "DtmfAudioSource",
                "placement": "node",
                "params": {
                    "rate_hz": 8000.0,
                    "digits": "1234",
                    "hold_ms": 200,
                    "gap_ms": 80,
                    "amplitude": 0.9,
                    "loop_sequence": false,
                },
            },
            "dtmf": {
                "type": "DtmfDecoder",
                "placement": "node",
                "params": {
                    "sample_rate_hz": 8000.0,
                    "block_size": 200,
                    "hold_ms": 40,
                    "off_ms": 40,
                    "dominance": 4.0,
                },
            },
            "events": {
                "type": "EventsSink",
                "placement": "node",
                "params": { "capacity": 1024 },
            },
        },
        "wires": [
            ["src.out", "dtmf.in"],
            ["dtmf.out", "events.in"],
        ],
    }))
    .expect("doc parses");

    let mut rt = Runtime::load_doc(
        &doc,
        Environment::Node,
        DEFAULT_FRAMES_HINT,
        DEFAULT_TICK_PERIOD,
    )
    .expect("runtime loads");
    rt.init().expect("init");
    rt.start().expect("start");

    // 4 digits × (200 ms hold + 80 ms gap) = 1120 ms. At 8 kS/s that
    // is 8 960 samples; with frames_hint=1024 the source delivers ≈9
    // ticks of audio. Add a healthy margin so the last digit's hold
    // has a full silence block to release into.
    for _ in 0..24 {
        rt.tick().expect("tick");
    }

    let events_sink = rt
        .block_typed::<EventsSink>("events")
        .expect("EventsSink instance accessible by id");
    let raw = events_sink.drain_events();
    assert!(
        !raw.is_empty(),
        "expected events buffered in the sink, got none"
    );

    let digits: Vec<String> = raw
        .iter()
        .map(|bytes| {
            let v: Value = serde_json::from_slice(bytes).unwrap_or_else(|_| {
                panic!("event is not JSON: {:?}", String::from_utf8_lossy(bytes))
            });
            v["digit"]
                .as_str()
                .expect("event has `digit` string field")
                .to_string()
        })
        .collect();
    assert_eq!(
        digits,
        vec!["1".to_string(), "2".into(), "3".into(), "4".into()],
        "expected [1,2,3,4] in order, got {digits:?}",
    );
    assert_eq!(events_sink.dropped(), 0, "no events should be dropped");
}

#[test]
fn dtmf_e2e_preset_splits_into_both_halves_cleanly() {
    // Load the doc straight off disk — this is also a contract test
    // that `flowgraphs/dtmf-e2e.json` stays parseable as the schema
    // evolves.
    let preset_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("flowgraphs/dtmf-e2e.json");
    let doc: FlowgraphDoc = serde_json::from_str(
        &std::fs::read_to_string(&preset_path).expect("dtmf-e2e.json readable"),
    )
    .expect("dtmf-e2e.json parses");

    // --- Node half ----------------------------------------------------
    let node =
        split_for_environment(&doc, Environment::Node, &InventorySpecRegistry).expect("node split");

    // Every authored node-side block survives.
    for id in ["src", "ammod", "tee", "chan", "decim", "fft", "logmag"] {
        assert!(
            node.blocks.contains_key(id),
            "node half missing {id}: {:?}",
            node.blocks.keys().collect::<Vec<_>>(),
        );
    }
    for id in ["demod", "dtmf", "events"] {
        assert!(
            !node.blocks.contains_key(id),
            "node half leaked browser block {id}",
        );
    }

    // Wire order: ui:fft is slot 0 (stream_id=1000); decim.out →
    // demod.in is slot 1 (stream_id=1001).
    let ui_tx_id = format!("__ui_fft_{CROSS_ENV_STREAM_BASE}");
    let cross_tx_id = format!("__bridge_tx_{}", CROSS_ENV_STREAM_BASE + 1);
    let ui_tx = node
        .blocks
        .get(&ui_tx_id)
        .unwrap_or_else(|| panic!("node half missing {ui_tx_id}"));
    assert_eq!(ui_tx.type_name, "WsBridgeTxFftU8");
    let cross_tx = node
        .blocks
        .get(&cross_tx_id)
        .unwrap_or_else(|| panic!("node half missing {cross_tx_id}"));
    assert_eq!(cross_tx.type_name, "WsBridgeTx");

    // --- Browser half -------------------------------------------------
    let browser = split_for_environment(&doc, Environment::Browser, &InventorySpecRegistry)
        .expect("browser split");

    for id in ["demod", "dtmf", "events"] {
        assert!(
            browser.blocks.contains_key(id),
            "browser half missing {id}: {:?}",
            browser.blocks.keys().collect::<Vec<_>>(),
        );
    }
    for id in ["src", "ammod", "tee", "chan", "decim", "fft", "logmag"] {
        assert!(
            !browser.blocks.contains_key(id),
            "browser half leaked node block {id}",
        );
    }
    // `ui:fft` consumes slot 0 but produces no rx-side block; only the
    // cross-env wire's rx lands on the browser half.
    let cross_rx_id = format!("__bridge_rx_{}", CROSS_ENV_STREAM_BASE + 1);
    let cross_rx = browser
        .blocks
        .get(&cross_rx_id)
        .unwrap_or_else(|| panic!("browser half missing {cross_rx_id}"));
    assert_eq!(cross_rx.type_name, "WsBridgeRx");

    // Bridge pair must share its stream_id — the whole point of
    // allocating from CROSS_ENV_STREAM_BASE in wire order.
    assert_eq!(
        bridge_stream_id(cross_tx),
        bridge_stream_id(cross_rx),
        "tx/rx pair must share stream_id",
    );
    assert_eq!(bridge_stream_id(cross_tx), CROSS_ENV_STREAM_BASE + 1);
}

fn bridge_stream_id(decl: &ferrite_runtime::BlockInstanceDecl) -> u32 {
    let raw = decl
        .params
        .as_ref()
        .and_then(|p| p.get("stream_id"))
        .and_then(Value::as_u64)
        .expect("bridge has stream_id param");
    u32::try_from(raw).expect("stream_id fits u32")
}
