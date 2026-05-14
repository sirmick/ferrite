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
use tokio::sync::{mpsc, oneshot, Mutex};

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
/// pane's registered snapshot fn, and replies with a [`ViewResponse`].
#[derive(Serialize)]
pub struct WireRequest<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str, // "view_request"
    pub req_id: u64,
    pub pane: &'a str,
}

/// JSON wire shape — browser → ferrited, on `/ws/ui-views`. `png_b64`
/// is the data-URL payload (raw base64, no `data:image/png,` prefix).
/// `error` is non-empty when the browser couldn't render (no canvas
/// registered for that pane, paused, etc.); in that case `png_b64` is
/// empty.
#[derive(Deserialize)]
pub struct WireResponse {
    pub req_id: u64,
    #[serde(default)]
    pub png_b64: String,
    #[serde(default)]
    pub error: String,
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
    pub async fn detach_viewer_if_current(&self, expected: &mpsc::Sender<ViewRequest>) {
        let mut slot = self.sender.lock().await;
        if let Some(s) = slot.as_ref() {
            if s.same_channel(expected) {
                *slot = None;
            }
        }
    }

    /// WS-side: complete a pending request. The browser's response
    /// payload arrived; decode base64 and resolve the oneshot. If the
    /// request_id is unknown (timed out or never existed), the result
    /// is dropped silently.
    pub async fn complete(&self, resp: WireResponse) {
        let Some(tx) = self.pending.lock().await.remove(&resp.req_id) else {
            return;
        };
        if !resp.error.is_empty() {
            // Drop the oneshot without sending — the request side
            // sees a timeout. Browser-error is also surfaced via the
            // server log so the operator sees what went wrong.
            tracing::warn!(
                target: "view_bridge",
                req_id = resp.req_id,
                "browser: {}", resp.error
            );
            return;
        }
        use base64::Engine as _;
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&resp.png_b64) {
            Ok(b) => b,
            Err(_) => return,
        };
        let _ = tx.send(bytes);
    }
}
