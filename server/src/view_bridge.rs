//! Server-side broker for `GET /api/ui-views/:pane` → browser → PNG.
//!
//! The four spectrum / waterfall canvases (wide spectrum, wide
//! waterfall, channel spectrum, channel waterfall) live in the
//! browser. `ferrite-ctl view <pane>` is the AI's way to grab one as a
//! rendered PNG — same content the operator is looking at, with the
//! band-plan overlay / VFO marker / contrast / pause state all baked
//! in by the live renderer rather than rebuilt from raw bins via
//! `fft_to_png.py`.
//!
//! Round-trip:
//!
//! ```text
//!   ferrite-ctl                ferrited               browser
//!   ───────────                ────────               ───────
//!   GET /api/ui-views/<pane>
//!     ────────────────────────►
//!                              allocate req_id
//!                              install oneshot
//!                              send WS msg
//!                              ───────────────────────►
//!                                                     toDataURL(...)
//!                              ◄───────────────────────
//!                                       JSON {req_id, png_b64}
//!                              decode + complete oneshot
//!     ◄────────────────────────
//!         200 OK, image/png
//! ```
//!
//! Single-viewer policy: matches D06 ("single listener, last-connect
//! wins") — when a new browser tab connects, it replaces the previous
//! one. Inflight requests against the old socket fail with a timeout
//! since the new socket won't see their req_ids.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

/// One pending HTTP request waiting on a browser snapshot.
type Pending = HashMap<u64, oneshot::Sender<Vec<u8>>>;

/// Pump command sent from the broker to the active WS task: "ask the
/// browser for `pane`, tag it with `req_id`". The WS task serialises
/// to JSON and sends; the response comes back through `complete()`.
pub struct ViewRequest {
    pub req_id: u64,
    pub pane: String,
}

/// JSON wire shape — ferrited → browser, on `/ws/ui-views`. The
/// browser-side `ViewBridge.svelte` decodes this, dispatches to the
/// pane's registered snapshot fn, and replies with a [`WireInbound::ViewResponse`].
#[derive(Serialize)]
pub struct WireRequest<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str, // "view_request"
    pub req_id: u64,
    pub pane: &'a str,
}

/// Cached snapshot of the browser's purely-visual chrome state.
///
/// The browser is the **author** for this state — every change pushed
/// over `/ws/ui-views` lands here verbatim. ferrited never originates a
/// value; it only caches what the browser tells it so `GET /api/view`
/// (and downstream MCP resources) can answer without round-tripping.
///
/// Fields are intentionally lean: only the bits the AI / external
/// drivers want to read or write. Scroll positions, hover, cursor —
/// none of that lives here.
///
/// All fields are nullable / optional so a partial push is meaningful;
/// the browser sends whatever it currently knows, and an absent field
/// means "browser declines to expose this" rather than "value is zero."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiViewState {
    /// Active left-panel tab: `"bands"` | `"catalog"` | `"settings"` |
    /// `"logs"` | `"flow"` | `"ai"`. `None` until the browser has
    /// connected and pushed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub left_tab: Option<String>,
    /// Whether the channel-detail pane (channel-spectrum +
    /// channel-waterfall) is currently visible.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel_detail_visible: Option<bool>,
    /// Per-pane zoom factor (1.0 = no zoom). Keys match the same pane
    /// names as the snapshot bridge: `wide-spectrum`, `wide-waterfall`,
    /// `channel-spectrum`, `channel-waterfall`.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub pane_zoom: HashMap<String, f64>,
    /// Per-pane pause state. Same key set as `pane_zoom`.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub pane_paused: HashMap<String, bool>,
}

/// JSON wire shape — browser → ferrited, on `/ws/ui-views`. Tagged
/// enum on the `type` field; `view_response` is the snapshot reply,
/// `view_state` is the periodic browser-authored chrome cache push.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireInbound {
    /// Reply to a [`WireRequest`] snapshot request. `png_b64` is the
    /// raw base64 payload (no `data:image/png,` prefix). `error` is
    /// non-empty when the browser couldn't render (no canvas
    /// registered for that pane, paused, etc.); in that case `png_b64`
    /// is empty.
    ViewResponse {
        req_id: u64,
        #[serde(default)]
        png_b64: String,
        #[serde(default)]
        error: String,
    },
    /// Browser-authored chrome cache push. Fire-and-forget: the
    /// browser emits this on every meaningful change to the state it
    /// surfaces, and ferrited overwrites its cache with the payload.
    ViewState { state: UiViewState },
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("no browser tab is subscribed to /ws/ui-views — open the UI first")]
    NoViewer,
    #[error("browser didn't respond within 3s (tab hidden? renderer paused?)")]
    Timeout,
}

/// Shared broker state. Cloning is cheap — everything is behind
/// `Arc<Mutex<...>>`.
#[derive(Clone, Default)]
pub struct ViewBridge {
    /// Active browser's sink. `None` when no tab is subscribed; replaced
    /// on each new subscriber (last-connect-wins).
    sender: Arc<Mutex<Option<mpsc::Sender<ViewRequest>>>>,
    /// In-flight HTTP requests, keyed by `req_id`. The WS handler
    /// completes them by looking up the id from the browser's response.
    pending: Arc<Mutex<Pending>>,
    /// Monotonic counter for `req_id`. Wraps on overflow — not a
    /// concern since inflight requests are short-lived (3 s timeout)
    /// and the wrap horizon is u64-sized.
    next_id: Arc<Mutex<u64>>,
    /// Last [`UiViewState`] the active browser pushed. `None` until the
    /// first `view_state` message lands; cleared on viewer detach so a
    /// stale snapshot doesn't outlive its source.
    state: Arc<RwLock<Option<UiViewState>>>,
}

impl ViewBridge {
    /// HTTP-side entry. Sends a request to the connected browser tab,
    /// waits up to 3 s for the PNG bytes back. The caller writes those
    /// bytes into the response body.
    pub async fn request(&self, pane: &str) -> Result<Vec<u8>, ViewError> {
        let Some(tx) = self.sender.lock().await.clone() else {
            return Err(ViewError::NoViewer);
        };
        let req_id = {
            let mut n = self.next_id.lock().await;
            *n = n.wrapping_add(1);
            *n
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending.lock().await.insert(req_id, resp_tx);

        if tx
            .send(ViewRequest {
                req_id,
                pane: pane.to_string(),
            })
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&req_id);
            return Err(ViewError::NoViewer);
        }

        match tokio::time::timeout(Duration::from_secs(3), resp_rx).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(_)) => {
                // Sender dropped (browser disconnected mid-flight).
                Err(ViewError::Timeout)
            }
            Err(_) => {
                self.pending.lock().await.remove(&req_id);
                Err(ViewError::Timeout)
            }
        }
    }

    /// WS-side: register a new subscriber. Returns the receiver the WS
    /// task pulls outbound requests from, plus a sender clone the
    /// caller hangs onto as an identity token for
    /// [`Self::detach_viewer_if_current`] on disconnect. Replaces any
    /// previous subscriber (last-connect-wins, per D06).
    pub async fn install_viewer(&self) -> (mpsc::Sender<ViewRequest>, mpsc::Receiver<ViewRequest>) {
        let (tx, rx) = mpsc::channel(16);
        *self.sender.lock().await = Some(tx.clone());
        (tx, rx)
    }

    /// WS-side: detach the current subscriber if it matches. Called on
    /// WS close. Idempotent — if a newer subscriber already replaced
    /// this one, we leave the newer one alone.
    ///
    /// Clears the cached [`UiViewState`] in the same step, so the next
    /// `GET /api/view` reports `NoViewer` instead of returning a stale
    /// snapshot from the now-gone browser.
    pub async fn detach_viewer_if_current(&self, expected: &mpsc::Sender<ViewRequest>) {
        let mut slot = self.sender.lock().await;
        if let Some(s) = slot.as_ref() {
            if s.same_channel(expected) {
                *slot = None;
                *self.state.write().await = None;
            }
        }
    }

    /// WS-side: dispatch a parsed inbound message. Splits between the
    /// snapshot reply path (resolves a pending oneshot) and the chrome
    /// state push (overwrites the cache).
    pub async fn ingest(&self, msg: WireInbound) {
        match msg {
            WireInbound::ViewResponse {
                req_id,
                png_b64,
                error,
            } => self.complete_response(req_id, &png_b64, &error).await,
            WireInbound::ViewState { state } => {
                *self.state.write().await = Some(state);
            }
        }
    }

    /// HTTP-side: read the last-known browser-authored chrome state.
    /// `None` means no viewer has connected and pushed yet; the route
    /// handler turns that into a 503 [`ViewError::NoViewer`] so callers
    /// see the same "open the UI first" semantics as the snapshot path.
    pub async fn state(&self) -> Option<UiViewState> {
        self.state.read().await.clone()
    }

    /// Internal: resolve a pending HTTP request with the browser's
    /// PNG bytes (or drop the oneshot on error, so the HTTP side times
    /// out cleanly). If the `req_id` is unknown (already timed out or
    /// never existed), the result is dropped silently.
    async fn complete_response(&self, req_id: u64, png_b64: &str, error: &str) {
        let Some(tx) = self.pending.lock().await.remove(&req_id) else {
            return;
        };
        if !error.is_empty() {
            // Drop the oneshot without sending — the request side
            // sees a timeout. Browser-error is also surfaced via the
            // server log so the operator sees what went wrong.
            tracing::warn!(
                target: "view_bridge",
                req_id,
                "browser: {error}",
            );
            return;
        }
        use base64::Engine as _;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(png_b64) {
            Ok(b) => b,
            Err(_) => return,
        };
        let _ = tx.send(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_starts_empty_until_pushed() {
        let bridge = ViewBridge::default();
        assert!(bridge.state().await.is_none(), "no push yet → None");

        bridge
            .ingest(WireInbound::ViewState {
                state: UiViewState {
                    left_tab: Some("ai".into()),
                    channel_detail_visible: Some(true),
                    pane_zoom: HashMap::from([("wide-spectrum".into(), 2.0)]),
                    pane_paused: HashMap::new(),
                },
            })
            .await;

        let got = bridge.state().await.expect("state cached after push");
        assert_eq!(got.left_tab.as_deref(), Some("ai"));
        assert_eq!(got.channel_detail_visible, Some(true));
        assert_eq!(got.pane_zoom.get("wide-spectrum"), Some(&2.0));
    }

    #[tokio::test]
    async fn detach_clears_state_only_for_current_viewer() {
        let bridge = ViewBridge::default();
        let (tx_a, _rx_a) = bridge.install_viewer().await;
        bridge
            .ingest(WireInbound::ViewState {
                state: UiViewState {
                    left_tab: Some("logs".into()),
                    ..Default::default()
                },
            })
            .await;
        assert!(bridge.state().await.is_some());

        // Newer viewer replaces tx_a — tx_a's "detach" must no-op so
        // we don't yank the newer viewer's cache out from under it.
        let (tx_b, _rx_b) = bridge.install_viewer().await;
        bridge.detach_viewer_if_current(&tx_a).await;
        assert!(
            bridge.state().await.is_some(),
            "stale-token detach must not clear the current viewer's cache",
        );

        // Current viewer detach clears.
        bridge.detach_viewer_if_current(&tx_b).await;
        assert!(bridge.state().await.is_none());
    }
}
