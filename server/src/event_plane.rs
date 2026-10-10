//! Event plane — the embedded MQTT broker and the one link that
//! publishes the store's record log onto it.
//!
//! The store (`DecoderStore`) is the source of truth; REST is the only
//! control plane. This module is the *fan-out*: every `StateDelta` the
//! store broadcasts becomes one MQTT message so any listener — the
//! browser, `ferrite-ctl tail`, an agent, a shell `mosquitto_sub` — can
//! subscribe to exactly the kinds it cares about with the same record
//! vocabulary `/api/state` returns. Nothing lives here that isn't
//! derivable from the store (plus the tracing log stream).
//!
//! Topic tree (`docs/plans/event-plane.md`):
//!
//! ```text
//! ferrite/state/<kind>          every delta of the kind (QoS 0). Replace
//!                               kinds: the latest record, retained.
//! ferrite/state/<kind>/<key>    keyed kinds: the entity's record, retained;
//!                               empty retained payload when it expires.
//! ferrite/state/log/<target>    tracing lines, `::` → `/` in the target.
//! ferrite/ingest/<kind>         anyone publishes a raw event; the server
//!                               folds it into the store (→ state/<kind>).
//! ```
//!
//! Payloads are the `StateDelta` JSON (`{kind, seq, epoch, op, record?,
//! key?}`) — the same reducer input the browser mirror and the REST
//! snapshot use, so a consumer needs one parser.
//!
//! The broker is `rumqttd` (MQTT 3.1.1 over TCP and WebSocket, own
//! ports, own threads — `Broker::start` blocks). Replace-kind bursts are
//! coalesced to ≤10 Hz per kind (latest wins); append and keyed records
//! are published as they happen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rumqttd::local::{LinkRx, LinkTx};
use rumqttd::protocol::{Packet, Publish};
use rumqttd::{Broker, Config, ConnectionSettings, Notification, RouterConfig, ServerSettings};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::decoder_store::{DecoderStore, DeltaOp, StateDelta};
use crate::log_stream::{LogBroadcast, LogEntry};

/// Topic prefix for store records.
pub const STATE_PREFIX: &str = "ferrite/state";
/// Topic prefix for raw events anyone may publish for the server to fold.
pub const INGEST_PREFIX: &str = "ferrite/ingest";
/// Minimum spacing between two publishes of the same replace kind.
const COALESCE_WINDOW: Duration = Duration::from_millis(100);

/// Running broker + publisher. Cheap to clone; the broker threads live
/// for the process.
#[derive(Debug, Clone)]
pub struct EventPlane {
    /// MQTT over TCP (v3.1.1) — `ferrite-ctl`, agents, `mosquitto_sub`.
    pub tcp: SocketAddr,
    /// MQTT over WebSocket (v3.1.1, subprotocol `mqtt`) — the browser,
    /// reached through ferrited's `/mqtt` reverse proxy.
    pub ws: SocketAddr,
}

impl EventPlane {
    /// Start the broker on `tcp` / `ws`, then the publisher (store
    /// deltas + log lines → topics) and the ingest folder
    /// (`ferrite/ingest/#` → store). Returns once the links are up; the
    /// listeners bind on their own threads moments later.
    pub fn start(
        store: Arc<DecoderStore>,
        logs: Option<LogBroadcast>,
        tcp: SocketAddr,
        ws: SocketAddr,
    ) -> Result<Self> {
        let broker = Broker::new(broker_config(tcp, ws));
        let (pub_tx, pub_rx) = broker
            .link("ferrited-state")
            .map_err(|e| anyhow!("mqtt publisher link: {e}"))?;
        let (ing_tx, ing_rx) = broker
            .link("ferrited-ingest")
            .map_err(|e| anyhow!("mqtt ingest link: {e}"))?;
        let mut broker = broker;
        std::thread::Builder::new()
            .name("mqtt-broker".into())
            .spawn(move || {
                if let Err(e) = broker.start() {
                    tracing::error!(error = %e, "mqtt broker stopped");
                }
            })
            .context("spawn mqtt broker thread")?;

        // Subscribe *here*, not in the tasks: a broadcast only reaches
        // receivers that exist when it's sent, so a record or log line
        // folded between `start` returning and the task's first poll
        // would otherwise be lost. Same for the ingest filter — a
        // publisher that connects right after us must be heard.
        let deltas = store.subscribe();
        let log_rx = logs.as_ref().map(LogBroadcast::subscribe);
        let mut ing_tx = ing_tx;
        ing_tx
            .subscribe(format!("{INGEST_PREFIX}/#"))
            .map_err(|e| anyhow!("mqtt ingest subscribe: {e}"))?;
        tokio::spawn(run_publisher(
            pub_tx,
            pub_rx,
            Arc::clone(&store),
            deltas,
            log_rx,
        ));
        tokio::spawn(run_ingest(ing_rx, store));
        tracing::info!(%tcp, %ws, "event plane up (mqtt)");
        Ok(Self { tcp, ws })
    }
}

fn broker_config(tcp: SocketAddr, ws: SocketAddr) -> Config {
    let connections = ConnectionSettings {
        connection_timeout_ms: 10_000,
        // A `graph` record carries the composed doc + browser half;
        // sample-rich decoder bursts are far smaller. 8 MiB is headroom.
        max_payload_size: 8 * 1024 * 1024,
        max_inflight_count: 1000,
        auth: None,
        external_auth: None,
        dynamic_filters: true,
    };
    let server = |name: &str, listen: SocketAddr| ServerSettings {
        name: name.to_owned(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections: connections.clone(),
    };
    Config {
        id: 0,
        router: RouterConfig {
            max_connections: 256,
            max_outgoing_packet_count: 1000,
            max_segment_size: 16 * 1024 * 1024,
            max_segment_count: 4,
            ..RouterConfig::default()
        },
        v4: Some(HashMap::from([("tcp".to_owned(), server("mqtt-tcp", tcp))])),
        ws: Some(HashMap::from([("ws".to_owned(), server("mqtt-ws", ws))])),
        ..Config::default()
    }
}

/// `ferrite/state/<kind>`.
pub fn kind_topic(kind: &str) -> String {
    format!("{STATE_PREFIX}/{kind}")
}

/// `ferrite/state/<kind>/<key>` — keys are sanitised so an APRS path or
/// a free-text id can't inject MQTT topic syntax.
pub fn key_topic(kind: &str, key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| if matches!(c, '/' | '+' | '#') { '_' } else { c })
        .collect();
    format!("{STATE_PREFIX}/{kind}/{safe}")
}

/// `ferrite/state/log/<target>` with `::` folded to `/` so
/// `decoder::pocsag` is `ferrite/state/log/decoder/pocsag` (one level
/// per module, so `log/decoder/#` is "every decoder").
pub fn log_topic(target: &str) -> String {
    format!("{STATE_PREFIX}/log/{}", target.replace("::", "/"))
}

async fn publish(tx: &mut LinkTx, topic: String, payload: String, retain: bool) {
    if let Err(e) = tx
        .send(Packet::Publish(Publish::new(topic, payload, retain), None))
        .await
    {
        tracing::warn!(error = %e, "mqtt publish failed");
    }
}

/// Per-kind bookkeeping the publisher needs that the delta itself
/// doesn't carry: which keys hold a retained entity (to clear them on
/// reset) and when a replace kind was last sent (coalescing).
#[derive(Default)]
struct PublisherState {
    retained_keys: HashMap<String, HashSet<String>>,
    last_sent: HashMap<String, Instant>,
    pending: BTreeMap<String, StateDelta>,
}

async fn handle_delta(tx: &mut LinkTx, st: &mut PublisherState, delta: StateDelta) {
    let kind = delta.kind.clone();
    match &delta.op {
        DeltaOp::Add { .. } => {
            publish(tx, kind_topic(&kind), delta.to_json(), false).await;
        }
        DeltaOp::Upsert { record } => match &record.key {
            Some(key) => {
                let json = delta.to_json();
                publish(tx, kind_topic(&kind), json.clone(), false).await;
                publish(tx, key_topic(&kind, key), json, true).await;
                st.retained_keys
                    .entry(kind)
                    .or_default()
                    .insert(key.clone());
            }
            None => {
                // Replace kind: retained whole record, ≤10 Hz per kind.
                let due = st
                    .last_sent
                    .get(&kind)
                    .is_none_or(|t| t.elapsed() >= COALESCE_WINDOW);
                if due {
                    st.last_sent.insert(kind.clone(), Instant::now());
                    publish(tx, kind_topic(&kind), delta.to_json(), true).await;
                } else {
                    st.pending.insert(kind, delta);
                }
            }
        },
        DeltaOp::Expire { key } => {
            publish(tx, kind_topic(&kind), delta.to_json(), false).await;
            publish(tx, key_topic(&kind, key), String::new(), true).await;
            if let Some(keys) = st.retained_keys.get_mut(&kind) {
                keys.remove(key);
            }
        }
        DeltaOp::Reset => {
            // Retained so a late subscriber of a replace kind sees "empty"
            // rather than the pre-reset record.
            st.pending.remove(&kind);
            publish(tx, kind_topic(&kind), delta.to_json(), true).await;
            if let Some(keys) = st.retained_keys.remove(&kind) {
                for key in keys {
                    publish(tx, key_topic(&kind, &key), String::new(), true).await;
                }
            }
        }
    }
}

async fn flush_pending(tx: &mut LinkTx, st: &mut PublisherState) {
    let pending = std::mem::take(&mut st.pending);
    for (kind, delta) in pending {
        st.last_sent.insert(kind.clone(), Instant::now());
        publish(tx, kind_topic(&kind), delta.to_json(), true).await;
    }
}

/// Re-publish everything retained from a fresh snapshot: replace kinds'
/// current record and every keyed entity. Append kinds have no retained
/// form (a late joiner takes `/api/state` for history). Used at boot
/// and after a lagged broadcast receiver (we may have missed deltas).
async fn republish_retained(tx: &mut LinkTx, st: &mut PublisherState, store: &DecoderStore) {
    let snap = store.snapshot();
    for (kind, state) in &snap.kinds {
        for (key, record) in &state.current {
            let delta = StateDelta {
                kind: kind.clone(),
                seq: record.seq,
                epoch: record.epoch,
                op: DeltaOp::Upsert {
                    record: record.clone(),
                },
            };
            match &record.key {
                Some(k) => {
                    publish(tx, key_topic(kind, k), delta.to_json(), true).await;
                    st.retained_keys
                        .entry(kind.clone())
                        .or_default()
                        .insert(k.clone());
                }
                None => {
                    debug_assert_eq!(key, "current");
                    publish(tx, kind_topic(kind), delta.to_json(), true).await;
                }
            }
        }
    }
}

async fn log_recv(rx: &mut Option<broadcast::Receiver<LogEntry>>) -> Option<LogEntry> {
    match rx {
        Some(rx) => loop {
            match rx.recv().await {
                Ok(e) => return Some(e),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

async fn run_publisher(
    mut tx: LinkTx,
    mut rx: LinkRx,
    store: Arc<DecoderStore>,
    mut deltas: broadcast::Receiver<StateDelta>,
    mut log_rx: Option<broadcast::Receiver<LogEntry>>,
) {
    let mut st = PublisherState::default();
    let mut flush = tokio::time::interval(COALESCE_WINDOW);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    republish_retained(&mut tx, &mut st, &store).await;

    loop {
        tokio::select! {
            d = deltas.recv() => match d {
                Ok(delta) => handle_delta(&mut tx, &mut st, delta).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "mqtt publisher lagged; republishing retained state");
                    st.pending.clear();
                    republish_retained(&mut tx, &mut st, &store).await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            entry = log_recv(&mut log_rx) => {
                let Some(entry) = entry else { log_rx = None; continue };
                // The broker's own lines would echo back through here
                // on every publish at debug level; keep them off the bus.
                if entry.target.starts_with("rumqttd") { continue; }
                let payload = serde_json::json!({
                    "target": entry.target,
                    "level": entry.level,
                    "message": entry.message,
                    "at_ms": entry.at_ms,
                });
                publish(&mut tx, log_topic(&entry.target), payload.to_string(), false).await;
            }
            _ = flush.tick() => {
                if !st.pending.is_empty() { flush_pending(&mut tx, &mut st).await; }
            }
            // A publish-only link still gets router notifications
            // (acks, unschedules); drain so its outgoing buffer never fills.
            n = rx.next() => match n {
                Ok(_) => {}
                Err(e) => { tracing::warn!(error = %e, "mqtt publisher link closed"); return; }
            },
        }
    }
}

/// `ferrite/ingest/<kind>` → `store.apply(kind, payload)`. The payload is
/// the raw event (what a block's `EventStore` would have folded); an
/// optional top-level `epoch` lets a publisher say which graph it
/// observed — a mismatch means a stale decoder, and the event is
/// dropped rather than mis-attributed.
async fn run_ingest(mut rx: LinkRx, store: Arc<DecoderStore>) {
    loop {
        match rx.next().await {
            Ok(Some(Notification::Forward(f))) => {
                let topic = String::from_utf8_lossy(&f.publish.topic).into_owned();
                let Some(kind) = topic.strip_prefix(&format!("{INGEST_PREFIX}/")) else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<Value>(&f.publish.payload) else {
                    tracing::debug!(%topic, "ingest: non-JSON payload dropped");
                    continue;
                };
                if let Some(e) = value.get("epoch").and_then(Value::as_u64) {
                    if e != store.epoch() {
                        tracing::debug!(%topic, epoch = e, current = store.epoch(), "ingest: stale epoch dropped");
                        continue;
                    }
                }
                // `epoch` is an envelope field (the record carries its own);
                // keep it out of `data` so browser- and node-placed decodes
                // of the same kind have the same shape.
                let mut value = value;
                if let Some(obj) = value.as_object_mut() {
                    obj.remove("epoch");
                }
                store.apply(kind, value);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "mqtt ingest link closed");
                return;
            }
        }
    }
}

impl StateDelta {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Bind-and-release a loopback port so two listeners (TCP + WS) can be
/// handed distinct free ports when the operator asked for "any".
pub fn free_loopback_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("probe free port")?;
    Ok(l.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet as CPacket, QoS};
    use std::collections::VecDeque;

    fn addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], free_loopback_port().unwrap()))
    }

    /// Subscribe `filter` and return a drain: `(topic, payload, retain)`
    /// tuples with a deadline, so the test can assert exact arrivals.
    async fn client(tcp: SocketAddr, filter: &str) -> (AsyncClient, rumqttc::EventLoop) {
        // Unique ids: a second client with the same id evicts the first.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = format!(
            "test-sub-{}",
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut opts = MqttOptions::new(id, tcp.ip().to_string(), tcp.port());
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut el) = AsyncClient::new(opts, 64);
        // The broker thread binds a beat after `start` returns; poll
        // until the CONNACK (rumqttc reconnects on refused connects).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline.into(), el.poll()).await {
                Ok(Ok(Event::Incoming(CPacket::ConnAck(_)))) => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => panic!("broker never accepted the test client"),
            }
        }
        client.subscribe(filter, QoS::AtMostOnce).await.unwrap();
        // `subscribe` only enqueues; wait for the SUBACK so a publish
        // issued right after this returns is actually routed to us.
        loop {
            match tokio::time::timeout_at(deadline.into(), el.poll()).await {
                Ok(Ok(Event::Incoming(CPacket::SubAck(_)))) => break,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("client error before suback: {e}"),
                Err(_) => panic!("no suback"),
            }
        }
        (client, el)
    }

    async fn drain(
        el: &mut rumqttc::EventLoop,
        want: usize,
        timeout: Duration,
    ) -> VecDeque<(String, Value, bool)> {
        let mut out = VecDeque::new();
        let deadline = Instant::now() + timeout;
        while out.len() < want {
            match tokio::time::timeout_at(deadline.into(), el.poll()).await {
                Ok(Ok(Event::Incoming(CPacket::Publish(p)))) => {
                    let payload = if p.payload.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&p.payload).unwrap()
                    };
                    out.push_back((p.topic, payload, p.retain));
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("client error: {e}"),
                Err(_) => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn retained_replace_record_reaches_a_late_subscriber() {
        let store = Arc::new(DecoderStore::new());
        store.bump_epoch();
        store.apply("graph", serde_json::json!({ "preset": "wbfm" }));
        let plane = EventPlane::start(Arc::clone(&store), None, addr(), addr()).unwrap();

        let (_c, mut el) = client(plane.tcp, "ferrite/state/graph").await;
        let got = drain(&mut el, 1, Duration::from_secs(3)).await;
        let (topic, payload, retain) = got.front().expect("retained graph");
        assert_eq!(topic, "ferrite/state/graph");
        assert!(retain, "replace kinds are retained");
        assert_eq!(payload["op"], "upsert");
        assert_eq!(payload["epoch"], 1);
        assert_eq!(payload["record"]["data"]["preset"], "wbfm");
    }

    #[tokio::test]
    async fn keyed_and_append_kinds_stream_and_retain_per_entity() {
        let store = Arc::new(DecoderStore::new());
        let plane = EventPlane::start(Arc::clone(&store), None, addr(), addr()).unwrap();
        let (_c, mut el) = client(plane.tcp, "ferrite/state/#").await;

        store.apply("adsb", serde_json::json!({ "icao": "abc123", "alt": 1000 }));
        store.apply("pager", serde_json::json!({ "msg": "hi" }));
        // A wildcard subscriber can see a retained publish twice on a
        // brand-new topic (rumqttd forwards it on the live path and again
        // as the filter's retained set); consumers dedup by `seq`, so the
        // assertions here are set-based with a time budget.
        let got = drain(&mut el, 8, Duration::from_millis(800)).await;
        let topics: Vec<&str> = got.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(topics.contains(&"ferrite/state/adsb"), "{topics:?}");
        assert!(topics.contains(&"ferrite/state/adsb/abc123"), "{topics:?}");
        assert!(topics.contains(&"ferrite/state/pager"), "{topics:?}");
        let keyed = got
            .iter()
            .find(|(t, _, _)| t == "ferrite/state/adsb/abc123")
            .unwrap();
        assert_eq!(keyed.1["record"]["key"], "abc123");
        let stream = got
            .iter()
            .find(|(t, _, _)| t == "ferrite/state/pager")
            .unwrap();
        assert!(!stream.2, "a live delivery never carries the retain flag");

        // Retention is what a *late* subscriber sees: the entity, flagged
        // retained; the append record is gone (history is the snapshot).
        let (_late, mut late_el) = client(plane.tcp, "ferrite/state/#").await;
        let late = drain(&mut late_el, 8, Duration::from_millis(800)).await;
        let entity = late
            .iter()
            .find(|(t, _, _)| t == "ferrite/state/adsb/abc123")
            .expect("entity retained for a late subscriber");
        assert!(entity.2, "retained delivery carries the retain flag");
        assert!(
            !late.iter().any(|(t, _, _)| t == "ferrite/state/pager"),
            "append kinds are not retained"
        );

        // Reset clears the kind on its topic and empties every entity.
        store.reset_kind("adsb");
        let got = drain(&mut el, 8, Duration::from_millis(800)).await;
        let reset = got
            .iter()
            .find(|(t, _, _)| t == "ferrite/state/adsb")
            .unwrap();
        assert_eq!(reset.1["op"], "reset");
        let cleared = got
            .iter()
            .find(|(t, _, _)| t == "ferrite/state/adsb/abc123")
            .expect("entity cleared");
        assert!(
            cleared.1.is_null(),
            "empty payload clears the retained entity"
        );
    }

    #[tokio::test]
    async fn replace_bursts_coalesce_to_the_latest_record() {
        let store = Arc::new(DecoderStore::new());
        let plane = EventPlane::start(Arc::clone(&store), None, addr(), addr()).unwrap();
        let (_c, mut el) = client(plane.tcp, "ferrite/state/rssi").await;
        // 20 updates inside one coalesce window → first goes out at once,
        // the rest collapse into one trailing publish with the last value.
        for i in 0..20 {
            store.apply("rssi", serde_json::json!({ "db": i }));
        }
        let got = drain(&mut el, 20, Duration::from_millis(600)).await;
        assert!(
            got.len() >= 2 && got.len() < 20,
            "coalesced: got {}",
            got.len()
        );
        assert_eq!(got.back().unwrap().1["record"]["data"]["db"], 19);
    }

    #[tokio::test]
    async fn ingest_folds_into_the_store_unless_the_epoch_is_stale() {
        let store = Arc::new(DecoderStore::new());
        store.bump_epoch(); // epoch 1
        let plane = EventPlane::start(Arc::clone(&store), None, addr(), addr()).unwrap();
        let (c, mut el) = client(plane.tcp, "ferrite/state/pager").await;
        c.publish(
            "ferrite/ingest/pager",
            QoS::AtMostOnce,
            false,
            r#"{"msg":"fresh","epoch":1}"#,
        )
        .await
        .unwrap();
        c.publish(
            "ferrite/ingest/pager",
            QoS::AtMostOnce,
            false,
            r#"{"msg":"stale","epoch":0}"#,
        )
        .await
        .unwrap();
        let got = drain(&mut el, 2, Duration::from_millis(800)).await;
        assert_eq!(got.len(), 1, "only the current-epoch event is folded");
        assert_eq!(got[0].1["record"]["data"]["msg"], "fresh");
        assert_eq!(store.snapshot_kind("pager").unwrap().recent.len(), 1);
    }

    #[tokio::test]
    async fn log_lines_publish_per_target() {
        let store = Arc::new(DecoderStore::new());
        let logs = LogBroadcast::new();
        let plane =
            EventPlane::start(Arc::clone(&store), Some(logs.clone()), addr(), addr()).unwrap();
        let (_c, mut el) = client(plane.tcp, "ferrite/state/log/decoder/#").await;
        logs.push_for_test(LogEntry {
            target: "decoder::pocsag".into(),
            level: "INFO".into(),
            message: "hello".into(),
            at_ms: 42,
        });
        logs.push_for_test(LogEntry {
            target: "ferrited::routes".into(),
            level: "INFO".into(),
            message: "not for us".into(),
            at_ms: 43,
        });
        let got = drain(&mut el, 2, Duration::from_millis(800)).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "ferrite/state/log/decoder/pocsag");
        assert_eq!(got[0].1["message"], "hello");
    }

    #[test]
    fn topics_are_sanitised() {
        assert_eq!(key_topic("aprs", "K6YIL-1"), "ferrite/state/aprs/K6YIL-1");
        assert_eq!(key_topic("aprs", "a/b+c#"), "ferrite/state/aprs/a_b_c_");
        assert_eq!(
            log_topic("decoder::pocsag"),
            "ferrite/state/log/decoder/pocsag"
        );
    }
}
