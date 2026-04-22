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

use std::time::Duration;

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
}
