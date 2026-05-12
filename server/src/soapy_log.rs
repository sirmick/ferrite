//! Bridge libSoapySDR's log handler into `tracing`.
//!
//! By default, every vendored SDR driver loaded under SoapySDR writes
//! its diagnostic messages straight to libSoapySDR's stderr-bound
//! logger. Those lines bypass `tracing` entirely — they don't reach
//! ferrited's `/ws/logs` broadcast, the `/api/decoder/recent` history
//! ring, or anything the AI sidecar can `Read`. They surface only to
//! whoever happens to be tailing the process's stderr.
//!
//! That hurts in two cases:
//!
//! - **Operator workflow**: the SDRplay driver emits
//!   `Not updating IFGR gain because AGC is enabled` /
//!   `sdrplay_api_OutOfRange` when a `setGain` call lands while AGC
//!   is on. The HTTP response for the offending tune still returns
//!   200 OK with the (silently-dropped) gain value; nothing in the
//!   API tells the caller it didn't take.
//! - **AI workflow**: same problem, worse. The AI sees a green
//!   tool result and concludes the gain change worked. The ground
//!   truth is in stderr where the AI can't see it.
//!
//! `SoapySDR_registerLogHandler` lets us install a callback for
//! every Soapy log message. We reify each into a `tracing::*!` event
//! under target `driver`, which the existing `LogBroadcast` layer
//! already pumps into the WS stream + history ring. From there
//! `tail decoder --category driver` works and the AI can poll for
//! warnings.
//!
//! Thread model: libSoapySDR invokes the callback on whatever thread
//! the originating Soapy call ran on. `tracing` macros are
//! thread-safe once `init()` has installed a subscriber, so this is
//! safe. We avoid any panic-able work inside the callback (no `?`,
//! no allocation that can fail loudly) since a panic across the FFI
//! boundary is UB.

use std::ffi::CStr;
use std::os::raw::c_char;

use soapysdr_sys::{
    SoapySDRLogLevel, SoapySDR_registerLogHandler, SOAPY_SDR_CRITICAL, SOAPY_SDR_DEBUG,
    SOAPY_SDR_ERROR, SOAPY_SDR_FATAL, SOAPY_SDR_INFO, SOAPY_SDR_NOTICE, SOAPY_SDR_SSI,
    SOAPY_SDR_TRACE, SOAPY_SDR_WARNING,
};

/// Install the Soapy-to-`tracing` bridge. Call once at process
/// startup, *after* `tracing` has its subscriber set (otherwise the
/// first events emitted by `SoapySDR_log` itself land in the void).
/// Idempotent at the SoapySDR layer — re-registering just replaces
/// the prior handler — but there's no reason to call twice.
pub fn install() {
    // SAFETY: `SoapySDR_registerLogHandler` only stores the function
    // pointer; it does not call the callback synchronously. Function
    // pointers to `extern "C"` functions are inherently safe to pass
    // across the FFI boundary.
    unsafe {
        SoapySDR_registerLogHandler(Some(soapy_log_callback));
    }
    tracing::info!(target: "driver", "SoapySDR log handler installed");
}

/// The C-side callback. Each Soapy log message → one tracing event
/// under target `driver`. We map Soapy's level enum to the closest
/// `tracing::Level`; SSI ("U" / "O" streaming-status indicators) gets
/// its own sub-target so callers can filter the high-volume
/// underflow/overflow chatter without dropping warnings.
unsafe extern "C" fn soapy_log_callback(level: SoapySDRLogLevel, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: contract of `SoapySDR_registerLogHandler` is that
    // `message` is a NUL-terminated C string valid for the duration
    // of the call. We do not retain the pointer past this scope.
    let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    match level {
        SOAPY_SDR_FATAL | SOAPY_SDR_CRITICAL | SOAPY_SDR_ERROR => {
            tracing::error!(target: "driver", "{msg}");
        }
        SOAPY_SDR_WARNING => {
            tracing::warn!(target: "driver", "{msg}");
        }
        SOAPY_SDR_NOTICE | SOAPY_SDR_INFO => {
            tracing::info!(target: "driver", "{msg}");
        }
        SOAPY_SDR_DEBUG => {
            tracing::debug!(target: "driver", "{msg}");
        }
        SOAPY_SDR_TRACE => {
            tracing::trace!(target: "driver", "{msg}");
        }
        SOAPY_SDR_SSI => {
            // Streaming Status Indicators — single-char strings like
            // "U" (underflow) or "O" (overflow). Useful for diagnosing
            // stream health but too noisy for the default `driver`
            // category. Route to a sub-target so a category filter of
            // `driver` still picks them up but `driver` excluding
            // `driver::ssi` is straightforward.
            tracing::info!(target: "driver::ssi", "{msg}");
        }
        other => {
            // Unknown level — surface as info so it's not lost but
            // tagged with the raw level for forensics.
            tracing::info!(target: "driver", "[lvl={other}] {msg}");
        }
    }
}
