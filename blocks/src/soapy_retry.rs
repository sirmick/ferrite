//! Shared open/retry/classify helpers for SoapySDR device access.
//!
//! Used by both:
//! - [`crate::soapy_source::SoapySource::try_construct`] — the block's
//!   construct path wraps a 4× retry loop over open + configure +
//!   activate, and decorates exhausted errors with driver-specific
//!   hints.
//! - `server::device::probe` — the capability probe runs the same
//!   transient-open race on SDRplay / RTL when a prior handle is still
//!   releasing. Before this module existed, the probe used a bare
//!   open and failed in cases where the block would have retried and
//!   succeeded.
//!
//! All classifiers take a **flat error chain string** rather than a
//! live `anyhow::Error`, because the probe path produces raw FFI
//! strings from `SoapySDRDevice_lastError()` and doesn't have an
//! `anyhow::Error` to walk. One routine, two callers, no coupling to
//! either side's error type.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Process-global settle gate between closing the last SoapySDR device
/// (`SoapySDRDevice_unmake`) and opening the next one.
///
/// The retry loops below are *reactive* — they let an open fail, then
/// back off and try again. But the dominant stop→start failure isn't
/// random: it's deterministic timing. SDRplay's `sdrplay_apiService`
/// daemon needs ~1 s after a stream deactivates before the next
/// `Device::new` will `activateStream()` cleanly; opened sooner, ~half
/// of cycles fail with `sdrplay_api_Fail` (this is documented in the
/// `device_lifecycle` stress test, which only sidesteps it because it
/// sleeps `FERRITE_TEST_DEVICE_GAP_MS` between iterations — the
/// production stop→start path had no equivalent gap). HackRF/USB has a
/// smaller but similar re-enumeration window.
///
/// This gate makes the wait *proactive* and pay-once: the close path
/// stamps a monotonic timestamp, and the open path sleeps out only the
/// remainder of the window. A cold first open (nothing closed yet) and
/// an open long after the last close both pay zero.
mod settle {
    use super::*;

    /// Monotonic process anchor. All close timestamps are nanos since
    /// this; a single `OnceLock` so the clock is shared even though
    /// `Instant` has no const ctor.
    fn anchor() -> Instant {
        static ANCHOR: OnceLock<Instant> = OnceLock::new();
        *ANCHOR.get_or_init(Instant::now)
    }

    /// Nanos-since-anchor of the most recent device close. `0` ==
    /// "no device has been closed in this process yet" (cold open).
    static LAST_CLOSE_NS: AtomicU64 = AtomicU64::new(0);

    /// Settle window. Default 1000 ms (matches the observed SDRplay
    /// daemon teardown). `FERRITE_SOAPY_SETTLE_MS` overrides it —
    /// `0` disables the gate entirely (faster hardware, RTL-only
    /// rigs, or stress runs that drive their own cadence).
    fn window() -> Duration {
        static MS: OnceLock<u64> = OnceLock::new();
        let ms = *MS.get_or_init(|| {
            std::env::var("FERRITE_SOAPY_SETTLE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1000)
        });
        Duration::from_millis(ms)
    }

    /// Record that a device was just unmade. Called from
    /// `SoapySource`'s drop path. `max(1)` keeps a close that lands
    /// at t≈anchor from reading back as the "never closed" sentinel.
    pub fn mark_closed() {
        let ns = anchor().elapsed().as_nanos() as u64;
        LAST_CLOSE_NS.store(ns.max(1), Ordering::SeqCst);
    }

    /// Block the calling thread until the settle window since the last
    /// close has elapsed. No-op on a cold open or once the window is
    /// already satisfied. Returns the duration actually slept so the
    /// caller can log it.
    pub fn wait() -> Duration {
        let last = LAST_CLOSE_NS.load(Ordering::SeqCst);
        if last == 0 {
            return Duration::ZERO;
        }
        let win = window();
        if win.is_zero() {
            return Duration::ZERO;
        }
        let since_close = anchor()
            .elapsed()
            .saturating_sub(Duration::from_nanos(last));
        let Some(remaining) = win.checked_sub(since_close) else {
            return Duration::ZERO;
        };
        if remaining.is_zero() {
            return Duration::ZERO;
        }
        std::thread::sleep(remaining);
        remaining
    }
}

/// Record that the last SoapySDR device handle was just released. Call
/// this immediately after a `soapysdr::Device` is dropped.
pub fn mark_device_closed() {
    settle::mark_closed();
}

/// Wait out the post-close settle window before the first open attempt.
/// See [`settle`] for why this is proactive rather than left to the
/// retry loop. Returns how long it actually slept (zero on a cold
/// open).
pub fn wait_settle_after_close() -> Duration {
    settle::wait()
}

/// Number of open attempts before we surface the error. Matches the
/// SoapySource construct loop.
pub const OPEN_MAX_ATTEMPTS: usize = 5;

/// Backoff between open attempts. Long enough for the SDRplay API
/// service to finish closing the previous handle (~200–400 ms observed)
/// and short enough that the total worst case stays under 2 s.
pub const OPEN_BACKOFF: Duration = Duration::from_millis(400);

/// True when `chain` — an error message flattened from an open or
/// activate failure — matches one of the "not ready yet, try again"
/// signatures we've actually observed on the dev hardware:
///
/// - `device deletion in-progress` — libSoapySDR returns this when a
///   previous handle is still being torn down inside the driver. Clears
///   within ~1 s on every driver we support.
/// - `RX stream already opened` — driver thinks a previous Rx stream
///   is still live. Also self-clears after a beat.
/// - `activate Rx stream :: NotSupported` *combined* — SDRplay's
///   `activateStream() - Init() failed: sdrplay_api_Fail` surfaces
///   through the safe wrapper as a contextless `NotSupported`.
/// - `sdrplay_api_Fail` — the same failure when it reaches us with
///   more context attached (raw FFI probe path).
#[must_use]
pub fn is_transient_open_chain(chain: &str) -> bool {
    chain.contains("device deletion in-progress")
        || chain.contains("RX stream already opened")
        || (chain.contains("activate Rx stream") && chain.contains("NotSupported"))
        || chain.contains("sdrplay_api_Fail")
}

/// Narrow case of [`is_transient_open_chain`] for the *open itself* —
/// "driver isn't ready for a new handle yet". Used by the raw
/// `SoapySDRDevice_makeStrArgs` retry in the probe path, which can't
/// trigger the activate-Rx-stream variants (there's no stream).
#[must_use]
pub fn is_transient_make_chain(chain: &str) -> bool {
    chain.contains("device deletion in-progress") || chain.contains("RX stream already opened")
}

/// Return a user-facing recovery hint when the error chain matches a
/// known wedge pattern. `args` is the SoapySDR args string so we can
/// tailor SDRplay-specific hints only when the SDRplay driver is
/// actually involved. Returns `None` for unfamiliar failures — they
/// surface verbatim without manufactured advice.
#[must_use]
pub fn hint_for_exhausted(chain: &str, args: &str) -> Option<&'static str> {
    if args.contains("driver=sdrplay")
        && (chain.contains("sdrplay_api_Fail")
            || (chain.contains("activate Rx stream") && chain.contains("NotSupported")))
    {
        return Some(
            "SDRplay API service appears wedged (activateStream keeps failing). \
             Try `sudo systemctl restart sdrplay` and retry.",
        );
    }
    if chain.contains("RX stream already opened") {
        return Some(
            "Driver still holds a previous Rx stream open. \
             Try replugging the SDR or restarting ferrited.",
        );
    }
    None
}

/// Render an `anyhow::Error`'s chain as the flat string that the
/// classifiers above consume.
#[must_use]
pub fn flatten_chain(err: &anyhow::Error) -> String {
    err.chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" :: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_device_deletion_race() {
        assert!(is_transient_make_chain(
            "device deletion in-progress, please wait"
        ));
        assert!(is_transient_open_chain(
            "device deletion in-progress, please wait"
        ));
    }

    #[test]
    fn classifies_sdrplay_api_fail() {
        // Raw FFI form (from SoapySDRDevice_lastError()).
        assert!(is_transient_open_chain(
            "sdrplay_api_Fail during activateStream Init"
        ));
        // Via safe wrapper: NotSupported with activate context.
        assert!(is_transient_open_chain(
            "activate Rx stream :: NotSupported: "
        ));
        // But plain NotSupported without the activate context does *not*
        // count — that's a genuine driver capability mismatch.
        assert!(!is_transient_open_chain(
            "set bandwidth=5000000 :: NotSupported"
        ));
    }

    #[test]
    fn hint_only_fires_on_sdrplay_args() {
        let chain = "activate Rx stream :: NotSupported: sdrplay_api_Fail";
        assert!(hint_for_exhausted(chain, "driver=sdrplay,serial=X").is_some());
        assert!(hint_for_exhausted(chain, "driver=hackrf").is_none());
    }

    #[test]
    fn unknown_chain_yields_no_hint() {
        assert!(hint_for_exhausted("some novel error", "driver=anything").is_none());
    }

    #[test]
    fn settle_gate_is_cold_then_armed() {
        // This is the only test that touches the settle module, so its
        // env read (cached in a OnceLock on first `window()` call) is
        // deterministic here. Short window keeps the test ~fast.
        std::env::set_var("FERRITE_SOAPY_SETTLE_MS", "120");

        // Cold: nothing closed yet → no wait.
        assert_eq!(wait_settle_after_close(), Duration::ZERO);

        // After a close, the very next open waits out (most of) the
        // window — bounded by it, and clearly non-trivial.
        mark_device_closed();
        let slept = wait_settle_after_close();
        assert!(
            slept > Duration::from_millis(50) && slept <= Duration::from_millis(120),
            "expected a sub-window settle wait, got {slept:?}"
        );

        // Immediately after the wait the window is satisfied → zero.
        assert_eq!(wait_settle_after_close(), Duration::ZERO);
    }
}
