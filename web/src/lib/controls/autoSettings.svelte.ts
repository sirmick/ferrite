// Frequency-band-conditioned auto-toggle for SDR driver settings.
//
// Some hardware notch filters (SDRplay's `rfnotch_ctrl`,
// `dabnotch_ctrl`) ship default-ON and quietly attenuate signals if
// the receiver tunes inside the band the notch covers. Each
// per-driver `sdr-presets/*.json` lists the rules: which `setting`
// key, which freq bands, and what value to write in vs out of band.
// `autoSettingsDelta` returns the diff against the source's current
// `settings` map; this module wires that into a `$effect` that fires
// on every centre-freq change.
//
// The same `pipeline.refreshFromServer()` plumbing the AI activity
// log triggers means an AI-driven tune (which mutates source via
// REST) flows back through this module the same way a UI click does
// — so the AI gets the auto-toggle for free without needing to set
// notches itself.

import { pipeline } from '$lib/pipeline.svelte';
import { autoSettingsDelta } from './optionsModel';
import { logs } from '$lib/logs/store.svelte';

/** Wire the auto-toggle effect. Call once at app mount; the returned
 *  cleanup is a no-op (Svelte effects in classes don't expose the
 *  inner unsubscribe, and we want this active for the app's lifetime
 *  anyway). */
export function installAutoSettingsEffect(): void {
  let lastApplied: { driver: string; freq: number } | null = null;
  let inFlight = false;

  $effect(() => {
    const src = pipeline.source;
    const caps = pipeline.sourceCaps;
    if (!src || !caps || caps.kind !== 'hardware') return;
    const driverKey = caps.capabilities.driver_key;
    const params = src.params as Record<string, unknown>;
    const freq = typeof params.center_freq_hz === 'number' ? params.center_freq_hz : NaN;
    if (!Number.isFinite(freq)) return;

    // Suppress redundant patches: re-run only when driver or freq
    // changed since last apply. Avoids re-triggering on every
    // unrelated source mutation (gain bump, antenna swap, etc.).
    if (lastApplied && lastApplied.driver === driverKey && Math.abs(lastApplied.freq - freq) < 1) {
      return;
    }

    const currentSettings = (params.settings as Record<string, string> | undefined) ?? {};
    const delta = autoSettingsDelta(driverKey, freq, currentSettings);
    lastApplied = { driver: driverKey, freq };

    if (Object.keys(delta).length === 0) return;
    if (inFlight) return; // pipeline.patchSource is busy-locked; let the next freq change retry

    inFlight = true;
    const merged = { ...currentSettings, ...delta };
    void pipeline.patchSourceParams({ settings: merged }).finally(() => {
      inFlight = false;
    });
    logs.push(
      'client',
      'info',
      `auto-settings: ${Object.entries(delta)
        .map(([k, v]) => `${k}=${v}`)
        .join(', ')} (centre ${(freq / 1e6).toFixed(3)} MHz, ${driverKey})`,
    );
  });
}
