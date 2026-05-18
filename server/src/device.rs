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
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use soapysdr_sys::{SoapySDRDevice, SoapySDRKwargs, SoapySDRRange};

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
pub(crate) fn list_devices() -> Result<Vec<DeviceInfo>> {
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
    /// Driver implements Soapy's automatic DC-offset-correction mode
    /// (`hasDCOffsetMode`). SDRplay does; SoapyHackRF does not. Drives
    /// whether the UI shows the DC-tracking toggle.
    pub has_dc_offset_mode: bool,
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
/// shot — one `SoapySDRDevice_makeStrArgs` / one `SoapySDRDevice_unmake`
/// per call, with every query (including `getSettingInfo`) in between.
///
/// Going straight through `soapysdr-sys` — not the safe `soapysdr` 0.5
/// wrapper — because that wrapper doesn't expose `getSettingInfo` and
/// has no way to hand out the underlying `*mut SoapySDRDevice`, so
/// mixing the two costs a second open per device. Drivers like RTL-SDR
/// and SDRplay do non-trivial work on every open (kernel detach, tuner
/// probe, API handshake), so we go direct and pay for it once.
pub(crate) fn probe(args: &str) -> Result<DeviceCapabilities> {
    let args_c = CString::new(args).with_context(|| format!("args contain NUL byte: {args:?}"))?;

    // Retry the raw `makeStrArgs` against the same "device deletion
    // in-progress" race that `SoapySource::open_with_retry` guards — a
    // prior handle (ours, from a recent probe, or the block's) may
    // still be tearing down inside the driver. Shared classifier in
    // `ferrite_blocks::soapy_retry` keeps the two paths in lockstep.
    let mut device_ptr: *mut SoapySDRDevice = std::ptr::null_mut();
    let mut last_err: Option<String> = None;
    for attempt in 0..ferrite_blocks::soapy_retry::OPEN_MAX_ATTEMPTS {
        // SAFETY: args_c outlives the call. A null return means open failed.
        device_ptr = unsafe { soapysdr_sys::SoapySDRDevice_makeStrArgs(args_c.as_ptr()) };
        if !device_ptr.is_null() {
            break;
        }
        let err = soapy_last_error();
        let is_last = attempt + 1 == ferrite_blocks::soapy_retry::OPEN_MAX_ATTEMPTS;
        if !is_last && ferrite_blocks::soapy_retry::is_transient_make_chain(&err) {
            tracing::warn!(
                attempt = attempt + 1,
                max = ferrite_blocks::soapy_retry::OPEN_MAX_ATTEMPTS,
                "SoapySDR probe open busy releasing; retrying"
            );
            std::thread::sleep(ferrite_blocks::soapy_retry::OPEN_BACKOFF);
            last_err = Some(err);
            continue;
        }
        bail!("open SoapySDR device with args {args:?}: {err}");
    }
    if device_ptr.is_null() {
        let err = last_err.unwrap_or_else(|| "open returned null with no error".into());
        bail!("open SoapySDR device with args {args:?}: {err}");
    }

    let guard = DeviceHandle(device_ptr);
    probe_with_handle(guard.0, args)
}

/// Runs all queries against an already-open device handle. Separate so
/// the RAII `DeviceHandle` still closes the device if any query fails.
fn probe_with_handle(device: *mut SoapySDRDevice, args: &str) -> Result<DeviceCapabilities> {
    let driver_key =
        unsafe { take_soapy_cstring(soapysdr_sys::SoapySDRDevice_getDriverKey(device)) };
    check_last_status().context("read driver_key")?;

    let hardware_key =
        unsafe { take_soapy_cstring(soapysdr_sys::SoapySDRDevice_getHardwareKey(device)) };
    check_last_status().context("read hardware_key")?;

    let mut hw_kwargs = unsafe { soapysdr_sys::SoapySDRDevice_getHardwareInfo(device) };
    check_last_status().context("read hardware_info")?;
    let hardware_info = kwargs_to_map(&hw_kwargs);
    // SAFETY: getHardwareInfo returns a Kwargs whose strings we own.
    unsafe { soapysdr_sys::SoapySDRKwargs_clear(&raw mut hw_kwargs) };

    let info = DeviceInfo {
        driver: driver_key.clone(),
        label: hardware_info
            .get("label")
            .cloned()
            .unwrap_or_else(|| driver_key.clone()),
        serial: hardware_info.get("serial").cloned(),
        args: parse_args_string(args),
    };

    let num_rx = unsafe { soapysdr_sys::SoapySDRDevice_getNumChannels(device, SOAPY_RX) };
    check_last_status().context("read num_channels(Rx)")?;

    let rx_channels = (0..num_rx)
        .map(|ch| probe_rx_channel(device, ch))
        .collect::<Result<Vec<_>>>()?;

    let settings = read_setting_info(device).unwrap_or_else(|err| {
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

/// Direction arg for the channel-keyed FFI entry points. The `soapysdr-sys`
/// constant is `u32`; the C API takes `c_int`.
#[allow(clippy::cast_possible_wrap)]
const SOAPY_RX: c_int = soapysdr_sys::SOAPY_SDR_RX as c_int;

/// RAII wrapper that guarantees `SoapySDRDevice_unmake` runs even if a
/// subsequent query fails.
struct DeviceHandle(*mut SoapySDRDevice);

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        // SAFETY: self.0 came from makeStrArgs and is closed exactly once.
        let rc = unsafe { soapysdr_sys::SoapySDRDevice_unmake(self.0) };
        if rc != 0 {
            tracing::debug!(rc, "SoapySDRDevice_unmake non-zero (ignored)");
        }
    }
}

/// Call immediately after a raw FFI call that doesn't return a status
/// code to surface any exception the C bindings caught.
fn check_last_status() -> Result<()> {
    // SAFETY: no aliasing; thread-local status + error.
    let status = unsafe { soapysdr_sys::SoapySDRDevice_lastStatus() };
    if status == 0 {
        Ok(())
    } else {
        bail!("{}", soapy_last_error())
    }
}

fn soapy_last_error() -> String {
    // SAFETY: Soapy keeps the last-error string in thread-local storage;
    // the pointer is valid until the next Device API call on this thread.
    let p = unsafe { soapysdr_sys::SoapySDRDevice_lastError() };
    if p.is_null() {
        "unknown SoapySDR error".to_string()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Enumerate `SoapySDRDevice_getSettingInfo` on an already-open device.
fn read_setting_info(device: *mut SoapySDRDevice) -> Result<Vec<SettingInfo>> {
    let mut len: usize = 0;
    // SAFETY: device non-null, len is a valid out-pointer.
    let list_ptr = unsafe { soapysdr_sys::SoapySDRDevice_getSettingInfo(device, &raw mut len) };
    check_last_status().context("read getSettingInfo")?;

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

fn parse_args_string(s: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn probe_rx_channel(device: *mut SoapySDRDevice, channel: usize) -> Result<RxChannelCapabilities> {
    let antennas = list_strings(device, channel, soapysdr_sys::SoapySDRDevice_listAntennas)
        .context("read antennas")?;

    let sample_rate_ranges_hz = list_ranges(
        device,
        channel,
        soapysdr_sys::SoapySDRDevice_getSampleRateRange,
    )
    .context("read sample-rate ranges")?;

    let bandwidth_ranges_hz = list_ranges(
        device,
        channel,
        soapysdr_sys::SoapySDRDevice_getBandwidthRange,
    )
    .context("read bandwidth ranges")?;

    let frequency_ranges_hz = list_ranges(
        device,
        channel,
        soapysdr_sys::SoapySDRDevice_getFrequencyRange,
    )
    .context("read frequency ranges")?;

    let components = list_strings(
        device,
        channel,
        soapysdr_sys::SoapySDRDevice_listFrequencies,
    )
    .context("read frequency components")?;

    let mut frequency_components = Vec::with_capacity(components.len());
    for name in components {
        let ranges = component_ranges(device, channel, &name, |dev, dir, ch, key, len| unsafe {
            soapysdr_sys::SoapySDRDevice_getFrequencyRangeComponent(dev, dir, ch, key, len)
        })
        .with_context(|| format!("read frequency range for component {name:?}"))?;
        frequency_components.push(FrequencyComponent {
            name,
            ranges_hz: ranges,
        });
    }

    let gain_names = list_strings(device, channel, soapysdr_sys::SoapySDRDevice_listGains)
        .context("read gain list")?;

    let mut gains = Vec::with_capacity(gain_names.len());
    for name in gain_names {
        let key = CString::new(name.as_str())
            .with_context(|| format!("gain name contains NUL: {name:?}"))?;
        // SAFETY: device is open, key lives for the call.
        let range = unsafe {
            soapysdr_sys::SoapySDRDevice_getGainElementRange(
                device,
                SOAPY_RX,
                channel,
                key.as_ptr(),
            )
        };
        check_last_status().with_context(|| format!("read gain range for element {name:?}"))?;
        gains.push(GainElement {
            name,
            range_db: range_from_raw(range),
        });
    }

    // SAFETY: device is open. getGainRange has no error surface separate
    // from lastStatus; treat a non-zero status as "no overall range".
    let overall_raw =
        unsafe { soapysdr_sys::SoapySDRDevice_getGainRange(device, SOAPY_RX, channel) };
    let overall_gain_range_db = if check_last_status().is_ok() {
        Some(range_from_raw(overall_raw))
    } else {
        None
    };

    // SAFETY: device is open.
    let has_agc = unsafe { soapysdr_sys::SoapySDRDevice_hasGainMode(device, SOAPY_RX, channel) };
    // hasGainMode sets the SoapySDR error status when the driver doesn't
    // support gain-mode control; treat that as "no AGC" rather than
    // surfacing it.
    let has_agc = check_last_status().is_ok() && has_agc;

    // SAFETY: device is open. Same status-swallow pattern as has_agc —
    // drivers lacking the interface set the error flag.
    let has_dc_offset_mode =
        unsafe { soapysdr_sys::SoapySDRDevice_hasDCOffsetMode(device, SOAPY_RX, channel) };
    let has_dc_offset_mode = check_last_status().is_ok() && has_dc_offset_mode;

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
        has_dc_offset_mode,
    })
}

type StringListFn =
    unsafe extern "C" fn(*const SoapySDRDevice, c_int, usize, *mut usize) -> *mut *mut c_char;

type RangeListFn =
    unsafe extern "C" fn(*const SoapySDRDevice, c_int, usize, *mut usize) -> *mut SoapySDRRange;

fn list_strings(
    device: *mut SoapySDRDevice,
    channel: usize,
    f: StringListFn,
) -> Result<Vec<String>> {
    let mut len: usize = 0;
    // SAFETY: device is open, len is a valid out-pointer.
    let ptr = unsafe { f(device, SOAPY_RX, channel, &raw mut len) };
    check_last_status()?;
    // SAFETY: Soapy returned `len` valid C strings on success.
    Ok(unsafe { take_soapy_string_list(ptr, len) })
}

fn list_ranges(
    device: *mut SoapySDRDevice,
    channel: usize,
    f: RangeListFn,
) -> Result<Vec<RangeSpec>> {
    let mut len: usize = 0;
    // SAFETY: device is open, len is a valid out-pointer.
    let ptr = unsafe { f(device, SOAPY_RX, channel, &raw mut len) };
    check_last_status()?;
    // SAFETY: Soapy returned `len` valid ranges on success.
    Ok(unsafe { take_soapy_range_list(ptr, len) })
}

fn component_ranges<F>(
    device: *mut SoapySDRDevice,
    channel: usize,
    name: &str,
    f: F,
) -> Result<Vec<RangeSpec>>
where
    F: FnOnce(*const SoapySDRDevice, c_int, usize, *const c_char, *mut usize) -> *mut SoapySDRRange,
{
    let key = CString::new(name).with_context(|| format!("component name NUL: {name:?}"))?;
    let mut len: usize = 0;
    let ptr = f(device, SOAPY_RX, channel, key.as_ptr(), &raw mut len);
    check_last_status()?;
    // SAFETY: Soapy returned `len` valid ranges on success.
    Ok(unsafe { take_soapy_range_list(ptr, len) })
}

fn range_from_raw(r: SoapySDRRange) -> RangeSpec {
    let step = if r.step > 0.0 { Some(r.step) } else { None };
    RangeSpec {
        min: r.minimum,
        max: r.maximum,
        step,
    }
}

fn kwargs_to_map(k: &SoapySDRKwargs) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if k.size == 0 || k.keys.is_null() || k.vals.is_null() {
        return map;
    }
    for i in 0..k.size {
        // SAFETY: keys/vals are `size` non-null C strings per Soapy contract.
        let key = unsafe { CStr::from_ptr(*k.keys.add(i)) }
            .to_string_lossy()
            .into_owned();
        let val = unsafe { CStr::from_ptr(*k.vals.add(i)) }
            .to_string_lossy()
            .into_owned();
        map.insert(key, val);
    }
    map
}

/// Turn a Soapy-allocated C string into an owned `String` and free the
/// backing buffer. Null input returns an empty string — matches the
/// previous safe-wrapper behaviour.
unsafe fn take_soapy_cstring(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: ptr came from a Soapy getter that expects SoapySDR_free.
    unsafe { soapysdr_sys::SoapySDR_free(ptr.cast::<c_void>()) };
    s
}

unsafe fn take_soapy_string_list(mut ptr: *mut *mut c_char, len: usize) -> Vec<String> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: caller guarantees `len` valid entries.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let out: Vec<String> = slice
        .iter()
        .map(|&p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
        .collect();
    // SAFETY: pairs the getter with its designated free function.
    unsafe { soapysdr_sys::SoapySDRStrings_clear(&raw mut ptr, len) };
    out
}

unsafe fn take_soapy_range_list(ptr: *mut SoapySDRRange, len: usize) -> Vec<RangeSpec> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: caller guarantees `len` valid ranges.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let out: Vec<RangeSpec> = slice.iter().copied().map(range_from_raw).collect();
    // SAFETY: range lists are freed with SoapySDR_free.
    unsafe { soapysdr_sys::SoapySDR_free(ptr.cast::<c_void>()) };
    out
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
        println!("    has_dc_offset_mode: {}", ch.has_dc_offset_mode);
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

/// Result of a short-duration sample read, used by `--read-all` to
/// confirm a device can actually produce samples after we've probed it.
#[derive(Debug, Clone, Serialize)]
pub struct SampleReadReport {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub bandwidth_hz: Option<f64>,
    pub gain_db: Option<f64>,
    pub antenna: Option<String>,
    pub samples_read: usize,
    pub elapsed_ms: u128,
    pub mean_magnitude: f32,
    pub peak_magnitude: f32,
    pub timeouts: u32,
    pub overflows: u32,
}

/// Optional per-call overrides for [`read_samples`]. `None` fields use
/// values derived from the device's probed capabilities.
#[derive(Debug, Clone, Default)]
pub struct ReadOverrides {
    pub sample_rate_hz: Option<f64>,
    pub bandwidth_hz: Option<f64>,
    pub center_freq_hz: Option<f64>,
    pub gain_db: Option<f64>,
}

/// Wrap [`read_samples`] in a worker thread with a wall-clock timeout so
/// a wedged driver (SDRplay API service, stuck USB handle) doesn't hang
/// the CLI. `read_for` is the sampling window itself; the thread is
/// given `read_for + 10s` of headroom for open/configure/activate.
pub fn read_samples_with_timeout(
    args: &str,
    caps: &DeviceCapabilities,
    read_for: Duration,
    overrides: ReadOverrides,
) -> Result<SampleReadReport> {
    let args_o = args.to_string();
    let caps_o = caps.clone();
    let budget = read_for + Duration::from_secs(10);
    with_probe_timeout(&format!("read_samples({args})"), budget, move || {
        read_samples(&args_o, &caps_o, read_for, &overrides)
    })
}

/// Open `args`, configure a sane sample rate / center frequency / gain
/// from `caps`, activate a `Complex<f32>` Rx stream on channel 0, and
/// read for `read_for`. Returns simple stats for the operator.
///
/// Crate-private so CLI callers are forced through
/// [`read_samples_with_timeout`] — a wedged driver otherwise hangs the
/// process forever.
pub(crate) fn read_samples(
    args: &str,
    caps: &DeviceCapabilities,
    read_for: Duration,
    overrides: &ReadOverrides,
) -> Result<SampleReadReport> {
    use num_complex::Complex;
    use soapysdr::{Direction, ErrorCode};
    use std::time::Instant;

    let rx = caps
        .rx_channels
        .first()
        .ok_or_else(|| anyhow::anyhow!("device has no Rx channels"))?;

    let sample_rate_hz = overrides
        .sample_rate_hz
        .or_else(|| pick_in_ranges(&rx.sample_rate_ranges_hz, 2_000_000.0))
        .ok_or_else(|| anyhow::anyhow!("device reports no sample-rate ranges"))?;
    let center_freq_hz = overrides
        .center_freq_hz
        .or_else(|| pick_in_ranges(&rx.frequency_ranges_hz, 100_000_000.0))
        .ok_or_else(|| anyhow::anyhow!("device reports no frequency ranges"))?;
    let gain_db = overrides
        .gain_db
        .or_else(|| rx.overall_gain_range_db.map(|r| (r.min + r.max) / 2.0));
    let antenna = rx.antennas.first().cloned();
    let channel = rx.index;

    let device = soapysdr::Device::new(args)
        .with_context(|| format!("open SoapySDR device with args {args:?}"))?;
    let dir = Direction::Rx;

    device
        .set_sample_rate(dir, channel, sample_rate_hz)
        .with_context(|| format!("set sample_rate={sample_rate_hz}"))?;
    device
        .set_frequency(dir, channel, center_freq_hz, ())
        .with_context(|| format!("set center_freq={center_freq_hz}"))?;
    if let Some(bw) = overrides.bandwidth_hz {
        device
            .set_bandwidth(dir, channel, bw)
            .with_context(|| format!("set bandwidth={bw}"))?;
    }
    if let Some(ant) = &antenna {
        // Single-antenna drivers accept this as a no-op; failure here is
        // not fatal to the read test.
        let _ = device.set_antenna(dir, channel, ant.as_bytes());
    }
    if let Some(g) = gain_db {
        // Some drivers reject the aggregate set_gain (HackRF does in some
        // builds); fall through to whatever default gain the driver uses.
        let _ = device.set_gain(dir, channel, g);
    }

    let mut stream = device
        .rx_stream::<Complex<f32>>(&[channel])
        .context("create Rx stream")?;
    stream.activate(None).context("activate Rx stream")?;

    let buf_size = 4096_usize;
    let mut buf = vec![Complex::new(0.0_f32, 0.0); buf_size];
    let mut samples_read = 0_usize;
    let mut mag_sum = 0.0_f64;
    let mut peak = 0.0_f32;
    let mut timeouts = 0_u32;
    let mut overflows = 0_u32;
    let start = Instant::now();
    let deadline = start + read_for;
    let read_timeout_us: i64 = 500_000;

    while Instant::now() < deadline {
        let result = {
            let mut slices: [&mut [Complex<f32>]; 1] = [&mut buf[..]];
            stream.read(&mut slices, read_timeout_us)
        };
        match result {
            Ok(0) => {}
            Ok(n) => {
                for s in &buf[..n] {
                    let m = (s.re * s.re + s.im * s.im).sqrt();
                    mag_sum += f64::from(m);
                    if m > peak {
                        peak = m;
                    }
                }
                samples_read = samples_read.saturating_add(n);
            }
            Err(err) if err.code == ErrorCode::Timeout => {
                timeouts = timeouts.saturating_add(1);
            }
            Err(err) if err.code == ErrorCode::Overflow => {
                overflows = overflows.saturating_add(1);
            }
            Err(err) => {
                let _ = stream.deactivate(None);
                bail!("stream read: {err}");
            }
        }
    }
    let elapsed = start.elapsed();
    let _ = stream.deactivate(None);

    let mean_magnitude = if samples_read > 0 {
        #[allow(clippy::cast_possible_truncation)]
        let mean = (mag_sum / samples_read as f64) as f32;
        mean
    } else {
        0.0
    };

    Ok(SampleReadReport {
        sample_rate_hz,
        center_freq_hz,
        bandwidth_hz: overrides.bandwidth_hz,
        gain_db,
        antenna,
        samples_read,
        elapsed_ms: elapsed.as_millis(),
        mean_magnitude,
        peak_magnitude: peak,
        timeouts,
        overflows,
    })
}

/// Pick the closest value to `target` that's valid across any of the
/// given ranges. Prefers `target` itself when a range contains it.
/// Returns `None` if `ranges` is empty.
fn pick_in_ranges(ranges: &[RangeSpec], target: f64) -> Option<f64> {
    if ranges.is_empty() {
        return None;
    }
    for r in ranges {
        if target >= r.min && target <= r.max {
            return Some(target);
        }
    }
    let mut best = ranges[0].min;
    let mut best_dist = (best - target).abs();
    for r in ranges {
        for cand in [r.min, r.max] {
            let d = (cand - target).abs();
            if d < best_dist {
                best = cand;
                best_dist = d;
            }
        }
    }
    Some(best)
}

pub fn print_sample_report(report: &SampleReadReport) {
    let gain = report
        .gain_db
        .map(|g| format!("{g:.1} dB"))
        .unwrap_or_else(|| "—".to_string());
    let bw = report
        .bandwidth_hz
        .map(|b| format!("{b:.0} Hz"))
        .unwrap_or_else(|| "—".to_string());
    let antenna = report.antenna.as_deref().unwrap_or("—");
    println!(
        "  rate={:.0} Hz  bw={bw}  center={:.0} Hz  gain={}  antenna={}",
        report.sample_rate_hz, report.center_freq_hz, gain, antenna,
    );
    let observed_msps = if report.elapsed_ms > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ms = report.elapsed_ms as f64;
        #[allow(clippy::cast_precision_loss)]
        let n = report.samples_read as f64;
        n / ms / 1000.0
    } else {
        0.0
    };
    println!(
        "  read {} samples in {} ms ({observed_msps:.2} Ms/s observed)",
        report.samples_read, report.elapsed_ms,
    );
    println!(
        "  mean|z|={:.4}  peak|z|={:.4}  timeouts={}  overflows={}",
        report.mean_magnitude, report.peak_magnitude, report.timeouts, report.overflows,
    );
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
                has_dc_offset_mode: true,
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
            "has_dc_offset_mode",
        ] {
            assert!(chan.get(key).is_some(), "missing channel key: {key}");
        }
        // Range step uses `null` for "continuous" (driven by `Option<f64>`).
        assert!(chan["sample_rate_ranges_hz"][0]["step"].is_null());
        assert_eq!(chan["gains"][0]["range_db"]["step"].as_f64(), Some(0.1));
        assert_eq!(chan["has_agc"], true);
        assert_eq!(chan["has_dc_offset_mode"], true);
    }
}
