//! `SoapySDR` device probe.
//!
//! Thin wrapper around [`soapysdr::enumerate`] that keeps the rest of
//! the server ignorant of `SoapySDR` types. Phase C callers are:
//!
//! - `ferrited --list-devices` (main.rs) — print on startup, exit 0.
//! - `GET /api/devices` (lands in #57) — JSON response to the browser.
//!
//! `libSoapySDR` is a hard build-time dependency — see `server/Cargo.toml`.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// Default probe timeout for the CLI. The "happy" SDRplay probe takes
/// ~2.3s on the dev box; we leave plenty of headroom for slower drivers
/// while still bailing long before a wedged `sdrplay_apiService` would
/// hang forever. Server routes set their own.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a probe-style closure on a worker thread and bail with a helpful
/// error if it doesn't return within `timeout`. SoapySDR drivers can
/// wedge at the C/C++ layer (e.g. the SDRplay API service holding a
/// stale device handle) and there's no way to cancel the in-flight call
/// — we leak the thread so the foreground stays responsive. Fine for
/// CLI use; the process exits right after.
pub fn with_probe_timeout<F, T>(label: &str, timeout: Duration, op: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel::<Result<T>>(1);
    thread::Builder::new()
        .name(format!("ferrite-probe[{label}]"))
        .spawn(move || {
            let _ = tx.send(op());
        })
        .context("spawn probe thread")?;
    match rx.recv_timeout(timeout) {
        Ok(res) => res,
        Err(mpsc::RecvTimeoutError::Timeout) => bail!(
            "{label} timed out after {:.1}s — SoapySDR may be wedged. \
             For SDRplay try `sudo systemctl restart sdrplay`; \
             for USB drivers replug the device. (The probe thread is \
             still running and will be cleaned up on process exit.)",
            timeout.as_secs_f64()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("{label} probe thread panicked before returning")
        }
    }
}

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

/// Same as [`list_devices`], but returns a clear timeout error rather
/// than hanging forever when a driver wedges at the C layer.
pub fn list_devices_with_timeout(timeout: Duration) -> Result<Vec<DeviceInfo>> {
    with_probe_timeout("list_devices", timeout, list_devices)
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

/// One driver-specific setting exposed via `SoapySDRDevice_getSettingInfo`.
///
/// These are the per-driver knobs that don't fit the standard Soapy
/// surface — e.g. `rfgain_sel`, `agc_setpoint`, `biasT_ctrl`, `hdr_ctrl`
/// on SDRplay; `direct_samp` on RTL-SDR; `amp_ctrl`, `bias_tx` on HackRF.
/// We pass them through verbatim so the frontend can render generic
/// controls without knowing the driver.
#[derive(Debug, Clone, Serialize)]
pub struct SettingInfo {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub units: Option<String>,
    pub data_type: SettingType,
    pub default: String,
    pub range: Option<RangeSpec>,
    pub options: Vec<SettingOption>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    Bool,
    Int,
    Float,
    String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingOption {
    pub value: String,
    pub label: Option<String>,
}

/// Full probe result: the enumeration info, one entry per Rx channel,
/// and any driver-specific settings reported by `getSettingInfo`. Tx is
/// out of scope for Ferrite.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCapabilities {
    pub info: DeviceInfo,
    pub driver_key: String,
    pub hardware_key: String,
    pub hardware_info: BTreeMap<String, String>,
    pub rx_channels: Vec<RxChannelCapabilities>,
    pub settings: Vec<SettingInfo>,
}

/// Same as [`probe`], with a hard wall-clock timeout. Use this from any
/// caller that can't tolerate an indefinite hang (CLI, HTTP handler).
pub fn probe_with_timeout(args: &str, timeout: Duration) -> Result<DeviceCapabilities> {
    let owned = args.to_string();
    with_probe_timeout(&format!("probe({args})"), timeout, move || probe(&owned))
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

    // Drop the safe handle before opening a second one for getSettingInfo.
    // Most drivers (RSPdx, RTL, HackRF) refuse a second concurrent handle.
    drop(device);
    let settings = read_setting_info(args).unwrap_or_else(|err| {
        tracing::debug!(?err, "getSettingInfo unavailable; settings list empty");
        Vec::new()
    });

    Ok(DeviceCapabilities {
        info,
        driver_key,
        hardware_key,
        hardware_info,
        rx_channels,
        settings,
    })
}

/// Open the device via raw FFI just long enough to enumerate
/// `SoapySDRDevice_getSettingInfo`. The safe `soapysdr` 0.5 wrapper
/// doesn't expose this call; rather than vendor a fork we reach into
/// `soapysdr-sys` for one cold-path query.
fn read_setting_info(args: &str) -> Result<Vec<SettingInfo>> {
    use std::ffi::CStr;

    // Re-use the safe Args parser; kwargs owns its own backing copy.
    let kwargs: soapysdr::Args = args.into();
    let kwargs_raw = kwargs.as_raw_const();

    // SAFETY: kwargs_raw points at kwargs's interior, alive for this call.
    let device = unsafe { soapysdr_sys::SoapySDRDevice_make(kwargs_raw) };
    if device.is_null() {
        let err_ptr = unsafe { soapysdr_sys::SoapySDRDevice_lastError() };
        let msg = if err_ptr.is_null() {
            "SoapySDRDevice_make returned null".to_string()
        } else {
            unsafe { CStr::from_ptr(err_ptr) }.to_string_lossy().into()
        };
        anyhow::bail!("open for getSettingInfo failed: {msg}");
    }

    let mut len: usize = 0;
    // SAFETY: device is non-null, len is a valid out-pointer.
    let list_ptr = unsafe { soapysdr_sys::SoapySDRDevice_getSettingInfo(device, &raw mut len) };

    let mut out = Vec::with_capacity(len);
    if !list_ptr.is_null() && len > 0 {
        // SAFETY: Soapy guarantees `len` valid `SoapySDRArgInfo` entries.
        let slice = unsafe { std::slice::from_raw_parts(list_ptr, len) };
        for raw in slice {
            out.push(setting_info_from_raw(raw));
        }
        // SAFETY: ArgInfoList_clear clears each entry's interior strings
        // *and* frees the list itself (TypesC.cpp:134) — no follow-up
        // SoapySDR_free needed.
        unsafe { soapysdr_sys::SoapySDRArgInfoList_clear(list_ptr, len) };
    }

    // SAFETY: device is the make() return value; closes once.
    let unmake_rc = unsafe { soapysdr_sys::SoapySDRDevice_unmake(device) };
    if unmake_rc != 0 {
        tracing::debug!(unmake_rc, "SoapySDRDevice_unmake non-zero (ignored)");
    }
    Ok(out)
}

fn setting_info_from_raw(raw: &soapysdr_sys::SoapySDRArgInfo) -> SettingInfo {
    use soapysdr_sys::{
        SOAPY_SDR_ARG_INFO_BOOL, SOAPY_SDR_ARG_INFO_FLOAT, SOAPY_SDR_ARG_INFO_INT,
        SOAPY_SDR_ARG_INFO_STRING,
    };

    // Driver-allocated C strings; lifetimes end with SoapySDRArgInfoList_clear.
    let key = unsafe { c_required(raw.key) };
    let default = unsafe { c_required(raw.value) };
    let name = unsafe { c_optional(raw.name) };
    let description = unsafe { c_optional(raw.description) };
    let units = unsafe { c_optional(raw.units) };

    let data_type = match raw.type_ {
        SOAPY_SDR_ARG_INFO_BOOL => SettingType::Bool,
        SOAPY_SDR_ARG_INFO_INT => SettingType::Int,
        SOAPY_SDR_ARG_INFO_FLOAT => SettingType::Float,
        SOAPY_SDR_ARG_INFO_STRING => SettingType::String,
        _ => SettingType::String,
    };

    let range = if matches!(data_type, SettingType::Int | SettingType::Float)
        && raw.range.maximum > raw.range.minimum
    {
        Some(RangeSpec::from(soapysdr::Range {
            minimum: raw.range.minimum,
            maximum: raw.range.maximum,
            step: raw.range.step,
        }))
    } else {
        None
    };

    let mut options = Vec::with_capacity(raw.numOptions);
    if !raw.options.is_null() && raw.numOptions > 0 {
        // SAFETY: Soapy guarantees `numOptions` valid C-string pointers.
        let vals = unsafe { std::slice::from_raw_parts(raw.options, raw.numOptions) };
        let names = if raw.optionNames.is_null() {
            None
        } else {
            // SAFETY: same length as `options` per Soapy contract.
            Some(unsafe { std::slice::from_raw_parts(raw.optionNames, raw.numOptions) })
        };
        for (i, &val_ptr) in vals.iter().enumerate() {
            let value = unsafe { c_required(val_ptr) };
            let label = names.and_then(|ns| unsafe { c_optional(ns[i]) });
            options.push(SettingOption { value, label });
        }
    }

    SettingInfo {
        label: name.clone().unwrap_or_else(|| key.clone()),
        key,
        description,
        units,
        data_type,
        default,
        range,
        options,
    }
}

unsafe fn c_required(p: *mut std::os::raw::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

unsafe fn c_optional(p: *mut std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned(),
    )
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
    if !caps.settings.is_empty() {
        println!();
        println!("  driver settings:");
        for s in &caps.settings {
            let kind = match s.data_type {
                SettingType::Bool => "bool",
                SettingType::Int => "int",
                SettingType::Float => "float",
                SettingType::String => "string",
            };
            let suffix = if let Some(r) = s.range {
                format!(" {}", format_range(r))
            } else if !s.options.is_empty() {
                let opts: Vec<&str> = s.options.iter().map(|o| o.value.as_str()).collect();
                format!(" {{{}}}", opts.join(","))
            } else {
                String::new()
            };
            println!("    {} ({kind}, default={}){}", s.key, s.default, suffix);
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

    /// The wrapper passes a fast-failing operation's error straight
    /// through — it should *not* dress it up as a timeout.
    #[test]
    fn with_probe_timeout_passes_underlying_error_through() {
        let res: Result<()> = with_probe_timeout("fast-fail", Duration::from_secs(5), || {
            bail!("inner failure")
        });
        let err = res.expect_err("expected failure");
        let msg = format!("{err:#}");
        assert!(msg.contains("inner failure"), "got: {msg}");
        assert!(!msg.contains("timed out"), "should not be a timeout: {msg}");
    }

    /// And a slow operation hits the timeout with the helpful hint.
    #[test]
    fn with_probe_timeout_fires_with_helpful_message() {
        let res: Result<()> = with_probe_timeout("slow-op", Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        let err = res.expect_err("expected timeout");
        let msg = format!("{err:#}");
        assert!(msg.contains("timed out"), "got: {msg}");
        assert!(
            msg.contains("sdrplay") || msg.contains("SDRplay"),
            "expected hint: {msg}"
        );
    }

    #[test]
    fn with_probe_timeout_returns_value_from_op() {
        let res = with_probe_timeout("ok", Duration::from_secs(5), || {
            Ok::<_, anyhow::Error>(42_u32)
        })
        .expect("should succeed");
        assert_eq!(res, 42);
    }

    /// Real-hardware probe — gated behind `--ignored` and the env var
    /// `FERRITE_TEST_DEVICE_ARGS`. Lets you exercise the exact code path
    /// the server uses, in isolation, with full stderr visible. Run e.g.
    /// `FERRITE_TEST_DEVICE_ARGS=driver=sdrplay \
    ///   cargo test -p ferrited --features soapysdr -- --ignored \
    ///   probe_real_device --nocapture`.
    #[test]
    #[ignore = "requires hardware; set FERRITE_TEST_DEVICE_ARGS to run"]
    fn probe_real_device() {
        let args = std::env::var("FERRITE_TEST_DEVICE_ARGS")
            .expect("set FERRITE_TEST_DEVICE_ARGS=driver=… to run this test");
        let caps = probe_with_timeout(&args, DEFAULT_PROBE_TIMEOUT)
            .expect("probe should succeed against real hardware");
        // Smoke checks — anything we'd expect from any sane SDR.
        assert!(!caps.driver_key.is_empty(), "driver_key empty");
        assert!(!caps.rx_channels.is_empty(), "no Rx channels reported");
        let ch0 = &caps.rx_channels[0];
        assert!(!ch0.antennas.is_empty(), "no antennas reported");
        assert!(
            !ch0.sample_rate_ranges_hz.is_empty(),
            "no sample-rate ranges reported"
        );
        assert!(
            !ch0.frequency_ranges_hz.is_empty(),
            "no frequency ranges reported"
        );
        // Print the probe so `--nocapture` doubles as a debug dump.
        print_capabilities(&caps);
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

    /// Lock the wire shape of the capability schema. The web option
    /// dialog (#64) parses this JSON, so any rename or restructure here
    /// is a breaking change that must update the frontend in lockstep.
    #[test]
    fn capability_schema_json_shape_is_stable() {
        let caps = DeviceCapabilities {
            info: DeviceInfo {
                driver: "rtlsdr".into(),
                label: "Generic RTL2832U".into(),
                serial: Some("00000001".into()),
                args: BTreeMap::from([
                    ("driver".to_string(), "rtlsdr".to_string()),
                    ("serial".to_string(), "00000001".to_string()),
                ]),
            },
            driver_key: "rtlsdr".into(),
            hardware_key: "RTL2832U".into(),
            hardware_info: BTreeMap::from([("tuner".to_string(), "R820T2".to_string())]),
            settings: vec![SettingInfo {
                key: "biasT_ctrl".into(),
                label: "BiasT Enable".into(),
                description: Some("BiasT Control".into()),
                units: None,
                data_type: SettingType::Bool,
                default: "true".into(),
                range: None,
                options: vec![],
            }],
            rx_channels: vec![RxChannelCapabilities {
                index: 0,
                antennas: vec!["RX".into()],
                sample_rate_ranges_hz: vec![RangeSpec {
                    min: 225_000.0,
                    max: 3_200_000.0,
                    step: None,
                }],
                bandwidth_ranges_hz: vec![],
                frequency_ranges_hz: vec![RangeSpec {
                    min: 24_000_000.0,
                    max: 1_766_000_000.0,
                    step: None,
                }],
                frequency_components: vec![FrequencyComponent {
                    name: "RF".into(),
                    ranges_hz: vec![RangeSpec {
                        min: 24_000_000.0,
                        max: 1_766_000_000.0,
                        step: None,
                    }],
                }],
                gains: vec![GainElement {
                    name: "TUNER".into(),
                    range_db: RangeSpec {
                        min: 0.0,
                        max: 49.6,
                        step: Some(0.1),
                    },
                }],
                overall_gain_range_db: Some(RangeSpec {
                    min: 0.0,
                    max: 49.6,
                    step: Some(0.1),
                }),
                has_agc: true,
            }],
        };
        let json = serde_json::to_value(&caps).expect("serialize");
        // Top-level keys
        for key in [
            "info",
            "driver_key",
            "hardware_key",
            "hardware_info",
            "rx_channels",
            "settings",
        ] {
            assert!(json.get(key).is_some(), "missing top-level key: {key}");
        }
        let setting = &json["settings"][0];
        for key in ["key", "label", "data_type", "default", "options"] {
            assert!(setting.get(key).is_some(), "missing setting key: {key}");
        }
        assert_eq!(setting["data_type"], "bool");
        let chan = &json["rx_channels"][0];
        for key in [
            "index",
            "antennas",
            "sample_rate_ranges_hz",
            "bandwidth_ranges_hz",
            "frequency_ranges_hz",
            "frequency_components",
            "gains",
            "overall_gain_range_db",
            "has_agc",
        ] {
            assert!(chan.get(key).is_some(), "missing channel key: {key}");
        }
        // Range step uses `null` for "continuous" (driven by `Option<f64>`).
        assert!(chan["sample_rate_ranges_hz"][0]["step"].is_null());
        assert_eq!(chan["gains"][0]["range_db"]["step"].as_f64(), Some(0.1));
        assert_eq!(chan["has_agc"], true);
    }
}
