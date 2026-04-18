//! `SoapySDR` device probe.
//!
//! Thin wrapper around [`soapysdr::enumerate`] that keeps the rest of
//! the server ignorant of `SoapySDR` types. Phase C callers are:
//!
//! - `ferrited --list-devices` (main.rs) — print on startup, exit 0.
//! - `GET /api/devices` (lands in #57) — JSON response to the browser.
//!
//! Gated on the `soapysdr` cargo feature so CI (and any hardware-free
//! build) compiles without `libSoapySDR` present.

#![cfg(feature = "soapysdr")]

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;

/// One `SoapySDR`-visible device and the kv args Soapy reports for it.
///
/// The `args` map carries the full, driver-specific key-value set
/// (e.g. `driver=sdrplay, label=RSP1A 1809071H07, serial=1809071H07`).
/// Common keys get lifted into named fields for ergonomic printing; the
/// raw map is retained so capability probes (#56) can see everything.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub driver: String,
    pub label: String,
    pub serial: Option<String>,
    pub args: BTreeMap<String, String>,
}

impl DeviceInfo {
    /// `driver=…,label=…` — the same form Soapy accepts in
    /// `Device::new(args)`, suitable for reopening this exact device.
    #[must_use]
    pub fn args_string(&self) -> String {
        self.args
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Enumerate every device `SoapySDR` plugins can see. Empty vec when
/// no hardware is attached — that's a clean success, not an error.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let entries = soapysdr::enumerate("").context("SoapySDR enumerate failed")?;
    Ok(entries.iter().map(device_info_from_args).collect())
}

fn device_info_from_args(args: &soapysdr::Args) -> DeviceInfo {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in args {
        map.insert(k.to_string(), v.to_string());
    }
    let driver = map.get("driver").cloned().unwrap_or_default();
    let label = map
        .get("label")
        .cloned()
        .or_else(|| map.get("device").cloned())
        .unwrap_or_else(|| driver.clone());
    let serial = map.get("serial").cloned();
    DeviceInfo {
        driver,
        label,
        serial,
        args: map,
    }
}

/// Pretty-print the result of [`list_devices`] to stdout, one block per
/// device. Used by the `--list-devices` CLI path.
pub fn print_devices(devices: &[DeviceInfo]) {
    if devices.is_empty() {
        println!("No SoapySDR devices found.");
        return;
    }
    println!("Found {} SoapySDR device(s):", devices.len());
    for (i, dev) in devices.iter().enumerate() {
        println!();
        println!("  [{i}] driver={} label={}", dev.driver, dev.label);
        if let Some(sn) = &dev.serial {
            println!("      serial={sn}");
        }
        println!("      args={}", dev.args_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Just asserts the call shape compiles and returns without panicking.
    // CI runs without hardware — an empty Vec is the expected happy path.
    #[test]
    fn list_devices_does_not_panic() {
        let _ = list_devices();
    }
}
