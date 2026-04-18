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

/// One min/max numeric range with an optional step hint. Mirrors
/// `SoapySDRRange` but `step == 0.0` is projected to `None`, which is
/// what Soapy uses to mean "continuous".
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RangeSpec {
    pub min: f64,
    pub max: f64,
    pub step: Option<f64>,
}

impl From<soapysdr::Range> for RangeSpec {
    fn from(r: soapysdr::Range) -> Self {
        let step = if r.step > 0.0 { Some(r.step) } else { None };
        Self {
            min: r.minimum,
            max: r.maximum,
            step,
        }
    }
}

/// Named gain stage (`"LNA"`, `"IF"`, …) and its allowed range in dB.
#[derive(Debug, Clone, Serialize)]
pub struct GainElement {
    pub name: String,
    pub range_db: RangeSpec,
}

/// Per-component (`"RF"`, `"BB"`) tuner range — Soapy splits the tuning
/// chain into named stages and each has its own achievable span.
#[derive(Debug, Clone, Serialize)]
pub struct FrequencyComponent {
    pub name: String,
    pub ranges_hz: Vec<RangeSpec>,
}

/// What one Rx channel on a device can do. The schema is intentionally
/// flat and allocation-happy — it's probed once at open and handed to
/// the web UI as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct RxChannelCapabilities {
    pub index: usize,
    pub antennas: Vec<String>,
    pub sample_rate_ranges_hz: Vec<RangeSpec>,
    pub bandwidth_ranges_hz: Vec<RangeSpec>,
    pub frequency_ranges_hz: Vec<RangeSpec>,
    pub frequency_components: Vec<FrequencyComponent>,
    pub gains: Vec<GainElement>,
    pub overall_gain_range_db: Option<RangeSpec>,
    pub has_agc: bool,
}

/// Full probe result: the enumeration info plus one entry per Rx
/// channel. Tx is out of scope for Ferrite.
///
/// # Driver-specific settings
///
/// `SoapySDRDevice_getSettingInfo` is not exposed by `rust-soapysdr`
/// 0.5, so bias-tee / IF-gain / RSP-antenna-routing type settings are
/// **not** in this schema yet. A follow-up commit will reach into
/// `soapysdr-sys` to fill that gap; the web option dialog (#64) will
/// need the same shape extended.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCapabilities {
    pub info: DeviceInfo,
    pub driver_key: String,
    pub hardware_key: String,
    pub hardware_info: BTreeMap<String, String>,
    pub rx_channels: Vec<RxChannelCapabilities>,
}

/// Open the device at `args`, query everything we can, close. Single
/// shot — Soapy serialises device access, so the caller can't hold the
/// returned struct and expect the device to stay open.
pub fn probe(args: &str) -> Result<DeviceCapabilities> {
    let device = soapysdr::Device::new(args)
        .with_context(|| format!("open SoapySDR device with args {args:?}"))?;

    let driver_key = device.driver_key().context("read driver_key")?.clone();
    let hardware_key = device.hardware_key().context("read hardware_key")?.clone();
    let hw_args = device.hardware_info().context("read hardware_info")?;
    let hardware_info = args_to_map(&hw_args);

    let info = DeviceInfo {
        driver: driver_key.clone(),
        label: hardware_info
            .get("label")
            .cloned()
            .unwrap_or_else(|| driver_key.clone()),
        serial: hardware_info.get("serial").cloned(),
        args: parse_args_string(args),
    };

    let num_rx = device
        .num_channels(soapysdr::Direction::Rx)
        .context("read num_channels(Rx)")?;
    let rx_channels = (0..num_rx)
        .map(|ch| probe_rx_channel(&device, ch))
        .collect::<Result<Vec<_>>>()?;

    Ok(DeviceCapabilities {
        info,
        driver_key,
        hardware_key,
        hardware_info,
        rx_channels,
    })
}

fn args_to_map(args: &soapysdr::Args) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (k, v) in args {
        map.insert(k.to_string(), v.to_string());
    }
    map
}

fn parse_args_string(s: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn probe_rx_channel(device: &soapysdr::Device, channel: usize) -> Result<RxChannelCapabilities> {
    let dir = soapysdr::Direction::Rx;

    let antennas = device.antennas(dir, channel).context("read antennas")?;

    let sample_rate_ranges_hz = device
        .get_sample_rate_range(dir, channel)
        .context("read sample-rate ranges")?
        .into_iter()
        .map(RangeSpec::from)
        .collect();

    let bandwidth_ranges_hz = device
        .bandwidth_range(dir, channel)
        .context("read bandwidth ranges")?
        .into_iter()
        .map(RangeSpec::from)
        .collect();

    let frequency_ranges_hz = device
        .frequency_range(dir, channel)
        .context("read frequency ranges")?
        .into_iter()
        .map(RangeSpec::from)
        .collect();

    let mut frequency_components = Vec::new();
    for comp in device
        .list_frequencies(dir, channel)
        .context("read frequency components")?
    {
        let ranges = device
            .component_frequency_range(dir, channel, comp.as_bytes())
            .with_context(|| format!("read frequency range for component {comp:?}"))?
            .into_iter()
            .map(RangeSpec::from)
            .collect();
        frequency_components.push(FrequencyComponent {
            name: comp,
            ranges_hz: ranges,
        });
    }

    let mut gains = Vec::new();
    for name in device.list_gains(dir, channel).context("read gain list")? {
        let range = device
            .gain_element_range(dir, channel, name.as_bytes())
            .with_context(|| format!("read gain range for element {name:?}"))?
            .into();
        gains.push(GainElement {
            name,
            range_db: range,
        });
    }

    let overall_gain_range_db = device.gain_range(dir, channel).ok().map(RangeSpec::from);
    let has_agc = device.has_gain_mode(dir, channel).unwrap_or(false);

    Ok(RxChannelCapabilities {
        index: channel,
        antennas,
        sample_rate_ranges_hz,
        bandwidth_ranges_hz,
        frequency_ranges_hz,
        frequency_components,
        gains,
        overall_gain_range_db,
        has_agc,
    })
}

/// Pretty-print the result of [`probe`]: one section per Rx channel,
/// mirroring roughly what `SoapySDRUtil --probe` produces.
pub fn print_capabilities(caps: &DeviceCapabilities) {
    println!(
        "driver={} hardware={} label={}",
        caps.driver_key, caps.hardware_key, caps.info.label,
    );
    if !caps.hardware_info.is_empty() {
        println!("  hardware_info:");
        for (k, v) in &caps.hardware_info {
            println!("    {k}={v}");
        }
    }
    for ch in &caps.rx_channels {
        println!();
        println!("  Rx channel {}:", ch.index);
        println!("    antennas: {}", ch.antennas.join(", "));
        print_ranges("sample_rate_hz", &ch.sample_rate_ranges_hz);
        print_ranges("bandwidth_hz", &ch.bandwidth_ranges_hz);
        print_ranges("frequency_hz", &ch.frequency_ranges_hz);
        for comp in &ch.frequency_components {
            print_ranges(&format!("freq.{}_hz", comp.name), &comp.ranges_hz);
        }
        if let Some(r) = ch.overall_gain_range_db {
            println!("    gain_total_db: {}", format_range(r));
        }
        println!("    has_agc: {}", ch.has_agc);
        for g in &ch.gains {
            println!("    gain.{}_db: {}", g.name, format_range(g.range_db));
        }
    }
}

fn print_ranges(label: &str, ranges: &[RangeSpec]) {
    if ranges.is_empty() {
        return;
    }
    let rendered: Vec<String> = ranges.iter().copied().map(format_range).collect();
    println!("    {label}: {}", rendered.join(", "));
}

fn format_range(r: RangeSpec) -> String {
    match r.step {
        Some(step) => format!("[{:.3}, {:.3}] step {:.3}", r.min, r.max, step),
        None => format!("[{:.3}, {:.3}]", r.min, r.max),
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

    #[test]
    fn probe_with_bogus_args_is_error_not_panic() {
        assert!(probe("driver=ferrite-no-such-driver").is_err());
    }

    #[test]
    fn parse_args_string_splits_on_comma_and_equals() {
        let map = parse_args_string("driver=sdrplay,label=RSP1A,serial=ABC123");
        assert_eq!(map.get("driver").map(String::as_str), Some("sdrplay"));
        assert_eq!(map.get("label").map(String::as_str), Some("RSP1A"));
        assert_eq!(map.get("serial").map(String::as_str), Some("ABC123"));
    }

    #[test]
    fn range_spec_zero_step_means_continuous() {
        let zero_step = soapysdr::Range {
            minimum: 1.0,
            maximum: 10.0,
            step: 0.0,
        };
        let stepped = soapysdr::Range {
            minimum: 1.0,
            maximum: 10.0,
            step: 0.5,
        };
        assert_eq!(RangeSpec::from(zero_step).step, None);
        assert_eq!(RangeSpec::from(stepped).step, Some(0.5));
    }
}
