//! Per-process cache of [`DeviceCapabilities`] keyed by stable device id.
//!
//! Probing a SoapySDR device is expensive (hundreds of ms for SDRplay,
//! plus a real risk of wedging the driver on rapid re-open) and the
//! result is essentially static — gain ranges, antenna lists, supported
//! sample rates don't change at runtime. We cache by serial when the
//! enumerate args expose one, falling back to the canonicalised args
//! string. The cache is cleared on explicit invalidate or pruned when
//! a key disappears from `enumerate`, so unplugging a device drops it.
//!
//! The probe call is injected for testability — production wiring uses
//! [`probe_with_timeout`]; unit tests pass a counting fake.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tokio::sync::RwLock;

use crate::device::{self, DeviceCapabilities, DeviceInfo};

/// Boxed probe function. The cache calls this on a miss; production
/// wires it to [`device::probe_with_timeout`], tests inject fakes.
pub type ProbeFn =
    Arc<dyn Fn(&str, Duration) -> Result<DeviceCapabilities> + Send + Sync + 'static>;

/// Stable id for one device — preferred form is `serial=<…>` so the
/// key survives the user reformatting an args string. Falls back to a
/// canonicalised `k=v,k=v` representation of the args when no serial
/// is known. This is the public keying rule; [`stable_device_key`] and
/// [`stable_device_key_from_args`] both produce it, and callers that
/// need to interop with the cache (e.g. building the `present` set
/// for [`DeviceCache::prune`]) should use one of those two.
fn canonical_key_from_map(map: &BTreeMap<String, String>) -> String {
    if let Some(serial) = map.get("serial").filter(|s| !s.is_empty()) {
        return format!("serial={serial}");
    }
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Stable cache key derived from a `DeviceInfo` (the enumerate output).
#[must_use]
pub fn stable_device_key(info: &DeviceInfo) -> String {
    if let Some(serial) = info.serial.as_deref().filter(|s| !s.is_empty()) {
        return format!("serial={serial}");
    }
    info.args_string()
}

/// Stable cache key derived from a raw SoapySDR args string — used
/// when the caller has no [`DeviceInfo`] (e.g. `/api/source/capabilities`
/// only knows the source's args). Re-parses + canonicalises so the
/// result matches [`stable_device_key`] for the same device regardless
/// of arg ordering.
#[must_use]
pub fn stable_device_key_from_args(args: &str) -> String {
    let map: BTreeMap<String, String> = args
        .split(',')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.trim().into(), v.trim().into()))
        })
        .collect();
    canonical_key_from_map(&map)
}

#[derive(Clone)]
pub struct DeviceCache {
    inner: Arc<RwLock<HashMap<String, DeviceCapabilities>>>,
    probe: ProbeFn,
    timeout: Duration,
}

impl DeviceCache {
    /// Cache wired to the real probe with the standard timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::with_probe(
            Arc::new(|args, t| device::probe_with_timeout(args, t)),
            device::DEFAULT_PROBE_TIMEOUT,
        )
    }

    /// Cache wired to an injected probe — used by tests.
    #[must_use]
    pub fn with_probe(probe: ProbeFn, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            probe,
            timeout,
        }
    }

    /// Return the cached capabilities for `info`, probing on miss. The
    /// probe runs on `tokio::task::spawn_blocking` because Soapy's C
    /// layer blocks. Concurrent calls for the same key may both probe
    /// (no per-key dedup yet) — fine for our cadence; the loser's
    /// result just overwrites in the map.
    pub async fn ensure(&self, info: &DeviceInfo) -> Result<DeviceCapabilities> {
        let key = stable_device_key(info);
        if let Some(hit) = self.inner.read().await.get(&key).cloned() {
            return Ok(hit);
        }
        let args = info.args_string();
        let probe = self.probe.clone();
        let timeout = self.timeout;
        let caps = tokio::task::spawn_blocking(move || probe(&args, timeout))
            .await
            .map_err(|e| anyhow::anyhow!("device probe task panicked: {e}"))??;
        self.inner.write().await.insert(key, caps.clone());
        Ok(caps)
    }

    /// Probe `args` directly when the caller has no `DeviceInfo` (e.g.
    /// `/api/source/capabilities` only knows the args string). Cache
    /// key is derived from the canonicalised args; re-probing the same
    /// device under a different arg ordering hits the same entry.
    pub async fn ensure_args(&self, args: &str) -> Result<DeviceCapabilities> {
        let key = stable_device_key_from_args(args);
        if let Some(hit) = self.inner.read().await.get(&key).cloned() {
            return Ok(hit);
        }
        let probe = self.probe.clone();
        let timeout = self.timeout;
        let owned = args.to_string();
        let caps = tokio::task::spawn_blocking(move || probe(&owned, timeout))
            .await
            .map_err(|e| anyhow::anyhow!("device probe task panicked: {e}"))??;
        self.inner.write().await.insert(key, caps.clone());
        Ok(caps)
    }

    /// Drop one cached entry — call when a probe-derived stat looks
    /// stale (e.g. a writeSetting changed available ranges).
    pub async fn invalidate(&self, key: &str) {
        self.inner.write().await.remove(key);
    }

    /// Drop every entry whose key isn't in `present` — call after a
    /// fresh `enumerate` so unplugged devices vanish from the cache.
    pub async fn prune(&self, present: &HashSet<String>) {
        let mut guard = self.inner.write().await;
        guard.retain(|k, _| present.contains(k));
    }

    /// Enumerate every visible device and probe each one into the
    /// cache. Intended to be called once at daemon boot so the first
    /// `/api/devices` request and the first `SoapySource::new` don't
    /// race against an unrelated probe. A failed probe is logged but
    /// doesn't abort warm-up — a wedged SDRplay shouldn't keep the
    /// rest of the rack offline.
    pub async fn warm_all(&self) -> WarmReport {
        let devices = match tokio::task::spawn_blocking(|| {
            crate::device::list_devices_with_timeout(crate::device::DEFAULT_PROBE_TIMEOUT)
        })
        .await
        {
            Ok(Ok(d)) => d,
            Ok(Err(err)) => {
                tracing::warn!(error = %format!("{err:#}"), "device enumerate failed during warm-up");
                return WarmReport::default();
            }
            Err(err) => {
                tracing::warn!(?err, "device enumerate task panicked during warm-up");
                return WarmReport::default();
            }
        };
        let mut report = WarmReport {
            attempted: devices.len(),
            ..WarmReport::default()
        };
        for info in devices {
            let label = info.label.clone();
            match self.ensure(&info).await {
                Ok(_) => {
                    tracing::info!(label = %label, "device cache warmed");
                    report.succeeded += 1;
                }
                Err(err) => {
                    tracing::warn!(label = %label, error = %format!("{err:#}"), "device probe failed during warm-up");
                    report.failed += 1;
                }
            }
        }
        report
    }

    /// Number of cached entries. Diagnostic only.
    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

/// Outcome of a [`DeviceCache::warm_all`] pass — printed at boot so
/// the operator can tell at a glance how many devices are usable.
#[derive(Debug, Default, Clone, Copy)]
pub struct WarmReport {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl Default for DeviceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceCapabilities, DeviceInfo, RxChannelCapabilities};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fake_caps(driver: &str) -> DeviceCapabilities {
        DeviceCapabilities {
            info: DeviceInfo {
                driver: driver.into(),
                label: driver.into(),
                serial: None,
                args: BTreeMap::new(),
            },
            driver_key: driver.into(),
            hardware_key: driver.into(),
            hardware_info: BTreeMap::new(),
            rx_channels: vec![RxChannelCapabilities {
                index: 0,
                antennas: vec![],
                sample_rate_ranges_hz: vec![],
                bandwidth_ranges_hz: vec![],
                frequency_ranges_hz: vec![],
                frequency_components: vec![],
                gains: vec![],
                overall_gain_range_db: None,
                has_agc: false,
            }],
            settings: vec![],
        }
    }

    fn info(driver: &str, serial: Option<&str>) -> DeviceInfo {
        let mut args = BTreeMap::new();
        args.insert("driver".into(), driver.into());
        if let Some(s) = serial {
            args.insert("serial".into(), s.into());
        }
        DeviceInfo {
            driver: driver.into(),
            label: driver.into(),
            serial: serial.map(String::from),
            args,
        }
    }

    fn counting_probe() -> (ProbeFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let probe: ProbeFn = Arc::new(move |args: &str, _t: Duration| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            // Echo the driver key from args so different devices look
            // distinct.
            let driver = args
                .split(',')
                .find_map(|p| p.strip_prefix("driver="))
                .unwrap_or("unknown");
            Ok(fake_caps(driver))
        });
        (probe, calls)
    }

    #[tokio::test]
    async fn ensure_probes_once_per_key() {
        let (probe, calls) = counting_probe();
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let info = info("rtlsdr", Some("00000001"));
        let _ = cache.ensure(&info).await.unwrap();
        let _ = cache.ensure(&info).await.unwrap();
        let _ = cache.ensure(&info).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn ensure_args_matches_ensure_when_serial_present() {
        // Both entry points should land on the same cache key for a
        // device with a serial — even if the args string is reordered.
        let (probe, calls) = counting_probe();
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let i = info("rtlsdr", Some("ABCDEF"));
        let _ = cache.ensure(&i).await.unwrap();
        // Reverse the arg order on the way in.
        let _ = cache
            .ensure_args("serial=ABCDEF,driver=rtlsdr")
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_forces_reprobe() {
        let (probe, calls) = counting_probe();
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let i = info("hackrf", Some("b74865"));
        let _ = cache.ensure(&i).await.unwrap();
        cache.invalidate(&stable_device_key(&i)).await;
        let _ = cache.ensure(&i).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn prune_drops_missing_keys() {
        let (probe, _) = counting_probe();
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let a = info("rtlsdr", Some("AAA"));
        let b = info("hackrf", Some("BBB"));
        let _ = cache.ensure(&a).await.unwrap();
        let _ = cache.ensure(&b).await.unwrap();
        assert_eq!(cache.len().await, 2);
        let mut present = HashSet::new();
        present.insert(stable_device_key(&a));
        cache.prune(&present).await;
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn fallback_key_uses_canonicalised_args_when_no_serial() {
        let (probe, calls) = counting_probe();
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let _ = cache
            .ensure_args("driver=fake,extra=z,more=a")
            .await
            .unwrap();
        // Same args, different order — should hit the same key.
        let _ = cache
            .ensure_args("more=a,extra=z,driver=fake")
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn probe_failure_is_not_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let probe: ProbeFn = Arc::new(move |_args: &str, _t: Duration| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("simulated probe failure")
        });
        let cache = DeviceCache::with_probe(probe, Duration::from_secs(1));
        let i = info("badsr", Some("X"));
        assert!(cache.ensure(&i).await.is_err());
        assert!(cache.ensure(&i).await.is_err());
        // Re-probed each time because failures don't populate the cache.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
