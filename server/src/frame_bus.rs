//! Per-subscriber frame fan-out.
//!
//! Replaces the `tokio::sync::broadcast::channel(32)` that used to sit
//! between the preset pipeline's `BroadcastSink` and the `/ws/preset`
//! WebSocket handler. That broadcast channel was 1→N with a single
//! shared ring: once the slowest subscriber fell more than 32 frames
//! behind, every subscriber started seeing [`Lagged`] errors and the
//! producer silently dropped frames for **all** of them. Cross-talk,
//!32-frame ceiling, no per-subscriber visibility.
//!
//! [`FrameBus`] keeps one bounded [`mpsc`] channel per subscriber.
//! [`FrameBus::send`] is sync (safe to call from the scheduler thread)
//! and fans out via `try_send` — a full per-subscriber queue loses
//! **that subscriber's** copy of the frame and no one else's, surfaces
//! as a `seq` gap on the wire, and logs a warning. Dead subscribers are
//! pruned on the next send. Buffer size is chosen at subscribe time so
//! the WS handler can size for its payload rate.
//!
//! Design notes:
//!
//! - The inner `Vec<Sub>` sits under a `std::sync::Mutex`. The scheduler
//!   thread holds it only for the duration of a fan-out; subscribers
//!   mutate it on connect/disconnect. Contention is near-zero in steady
//!   state.
//! - `try_send` is used on every fan-out hop. We never await in `send`
//!   because the scheduler thread would otherwise stall the entire
//!   pipeline when one subscriber pauses.
//! - `Closed` senders (the `Receiver` was dropped — usually a client
//!   disconnect) are pruned immediately so `subs` stays proportional to
//!   live subscribers.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use tokio::sync::mpsc;

use crate::app_state::FrameBytes;

/// Default per-subscriber queue depth. At 2.4 MS/s IQ with
/// ~1000-sample ticks, 1024 slots is ~400 ms of headroom — plenty to
/// absorb async-scheduling hiccups, far short of unbounded growth.
pub const DEFAULT_SUBSCRIBER_CAPACITY: usize = 1024;

struct Sub {
    id: u64,
    tx: mpsc::Sender<FrameBytes>,
}

#[derive(Clone)]
pub struct FrameBus {
    inner: Arc<Mutex<Vec<Sub>>>,
}

impl FrameBus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Subscribe with a per-subscriber bounded queue. The returned
    /// `Receiver` is dropped by the caller on disconnect, which signals
    /// the bus to prune the sender on the next send.
    pub fn subscribe(&self, capacity: usize) -> mpsc::Receiver<FrameBytes> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(capacity);
        self.inner
            .lock()
            .expect("FrameBus lock")
            .push(Sub { id, tx });
        tracing::debug!(id, capacity, "frame bus: subscribed");
        rx
    }

    /// Fan out a frame to every live subscriber. Sync; callable from
    /// the scheduler. Per-subscriber `try_send` — a full queue drops
    /// only *that* subscriber's copy. Closed senders are pruned.
    pub fn send(&self, bytes: FrameBytes) {
        let mut subs = self.inner.lock().expect("FrameBus lock");
        subs.retain(|sub| match sub.tx.try_send(bytes.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    id = sub.id,
                    "frame bus: subscriber full, dropping one frame"
                );
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(id = sub.id, "frame bus: subscriber gone, pruned");
                false
            }
        });
    }

    /// Count live subscribers. Cheap — for tests + status endpoints.
    #[must_use]
    #[allow(dead_code)]
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().expect("FrameBus lock").len()
    }
}

impl Default for FrameBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(n: u8) -> FrameBytes {
        Arc::new(vec![n])
    }

    #[tokio::test]
    async fn single_subscriber_receives_in_order() {
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(8);
        for i in 0..4u8 {
            bus.send(payload(i));
        }
        for i in 0..4u8 {
            let got = rx.recv().await.expect("recv");
            assert_eq!(got.as_slice(), &[i]);
        }
    }

    #[tokio::test]
    async fn fanout_sends_to_every_subscriber() {
        let bus = FrameBus::new();
        let mut rx0 = bus.subscribe(8);
        let mut rx1 = bus.subscribe(8);
        bus.send(payload(42));
        assert_eq!(rx0.recv().await.unwrap().as_slice(), &[42]);
        assert_eq!(rx1.recv().await.unwrap().as_slice(), &[42]);
    }

    #[tokio::test]
    async fn slow_subscriber_does_not_affect_fast_one() {
        // A slow subscriber with capacity 2 that never drains must not
        // cause frames to go missing for a fast subscriber with
        // capacity 16.
        let bus = FrameBus::new();
        let _slow = bus.subscribe(2);
        let mut fast = bus.subscribe(16);
        for i in 0..8u8 {
            bus.send(payload(i));
        }
        for i in 0..8u8 {
            let got = fast.recv().await.unwrap();
            assert_eq!(got.as_slice(), &[i]);
        }
    }

    #[tokio::test]
    async fn full_queue_drops_frame_but_keeps_subscriber() {
        // Capacity 2, send 5 without draining — only the first 2 land,
        // but the subscriber stays on the bus and can still receive
        // later sends after draining.
        let bus = FrameBus::new();
        let mut rx = bus.subscribe(2);
        for i in 0..5u8 {
            bus.send(payload(i));
        }
        assert_eq!(rx.recv().await.unwrap().as_slice(), &[0]);
        assert_eq!(rx.recv().await.unwrap().as_slice(), &[1]);
        // Drained — now the bus can send again.
        bus.send(payload(99));
        assert_eq!(rx.recv().await.unwrap().as_slice(), &[99]);
        assert_eq!(bus.subscriber_count(), 1, "subscriber not pruned on full");
    }

    #[tokio::test]
    async fn dropped_receiver_is_pruned() {
        let bus = FrameBus::new();
        let rx = bus.subscribe(4);
        assert_eq!(bus.subscriber_count(), 1);
        drop(rx);
        // Prune happens on the next send.
        bus.send(payload(1));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn send_with_no_subscribers_is_noop() {
        let bus = FrameBus::new();
        bus.send(payload(1)); // must not panic
        assert_eq!(bus.subscriber_count(), 0);
    }
}
