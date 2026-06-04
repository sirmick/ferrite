<script lang="ts">
  // Advanced input panel — every capability the probe surfaces, one
  // control per key, no smart logic. Each change writes its own key via
  // `patchSourceParams`; the server routes hot-apply vs rebuild by key.
  // The curated "quick" controls next to the nixies are the smart twin
  // (e.g. sample-rate change also applies the ladder BW atomically); this
  // panel stays deliberately dumb so what you pick is what the driver sees.
  //
  // Tooltips surface the probe metadata verbatim (`SettingInfo.description`,
  // `RangeSpec` bounds, gain element name) so the user can inspect what
  // each knob actually is without leaving the UI.

  import { pipeline } from '$lib/pipeline.svelte';
  import type { RangeSpec, SettingInfo, SettingType } from '$lib/api/devices';
  import {
    fullBandwidthChoices,
    fullRateChoices,
    gainDescriptionFor,
    gainLabelFor,
    hiddenSettingsFor,
    overallGainRangeFor,
    settingOverridesFor,
    type SettingOverride,
  } from './optionsModel';
  import { applyControl } from '$lib/control/dispatch';

  let caps = $derived(
    pipeline.sourceCaps?.kind === 'hardware' ? pipeline.sourceCaps.capabilities : null,
  );
  let params = $derived((pipeline.source?.params ?? {}) as Record<string, unknown>);
  let pending = $state<string | null>(null);
  let busy = $derived(pipeline.phase === 'busy');

  let settingsMap = $derived((params.settings ?? {}) as Record<string, string>);
  let channel = $derived(caps?.rx_channels[0] ?? null);

  let rateChoices = $derived(caps ? fullRateChoices(caps) : []);
  let bwChoices = $derived(caps ? fullBandwidthChoices(caps) : []);
  let hiddenSettings = $derived(new Set(caps ? hiddenSettingsFor(caps) : []));
  // Driver-specific UI overrides for misleading upstream labels — see
  // `SettingOverride` in optionsModel.ts. The lookup happens per-render
  // through `effectiveLabel` / `effectiveDescription` so the override
  // JSON is the single source of truth without per-key Svelte plumbing.
  let settingOverrides = $derived<Record<string, SettingOverride>>(
    caps ? settingOverridesFor(caps) : {},
  );
  // Drop preset-hidden keys.
  let visibleSettings = $derived((caps?.settings ?? []).filter((s) => !hiddenSettings.has(s.key)));

  function effectiveLabel(s: SettingInfo): string {
    return settingOverrides[s.key]?.label ?? s.label;
  }
  function effectiveDescription(s: SettingInfo): string | undefined {
    return settingOverrides[s.key]?.description ?? s.description ?? undefined;
  }
  function effectiveOptionLabel(s: SettingInfo, optValue: string, optLabel: string | null): string {
    return settingOverrides[s.key]?.option_labels?.[optValue] ?? optLabel ?? optValue;
  }

  let freqRange = $derived(channel?.frequency_ranges_hz[0] ?? null);
  let overallGainRange = $derived(caps ? overallGainRangeFor(caps) : null);
  let antennas = $derived(channel?.antennas ?? []);
  let hasAgc = $derived(channel?.has_agc ?? false);
  let hasDcOffset = $derived(channel?.has_dc_offset_mode ?? false);
  // Driver-specific master-gain label/tooltip — see gainLabelFor /
  // gainDescriptionFor in optionsModel.ts. SDRplay's "overall" gain
  // only spans IFGR; we relabel to "IF gain (dB)" so users don't expect
  // it to drive the LNA stage too.
  let gainLabel = $derived(caps ? gainLabelFor(caps) : 'gain (dB)');
  let gainDescription = $derived(caps ? gainDescriptionFor(caps) : undefined);

  let currentRate = $derived(numberOr(params.sample_rate_hz, rateChoices[0] ?? NaN));
  let currentBw = $derived(numberOr(params.bandwidth_hz, NaN));
  let currentFreqMHz = $derived(numberOr(params.center_freq_hz, NaN) / 1e6);
  let currentGain = $derived(numberOr(params.gain_db, overallGainRange?.min ?? 0));
  // The wire `gain_db` is already user-facing (high = more amplification);
  // SoapySource inverts at the boundary for reduction-shaped drivers
  // (SDRplay), so the slider binds straight to the value.
  let gainDisplay = $derived(currentGain);
  // Per-stage gain elements (HackRF: LNA / VGA / AMP) for the Advanced
  // manual panel and the convenience-dial AMP indicator.
  let gainStages = $derived(channel?.gains ?? []);
  let gainMode = $derived(
    typeof params.gain_mode === 'string' ? (params.gain_mode as string) : 'auto',
  );
  let manualGain = $derived(gainMode === 'manual');
  let gainElementValues = $derived((params.gain_elements ?? {}) as Record<string, number>);
  // The easily-overloaded +14 dB front-end amp (a 0/14 stage named "AMP").
  // In auto it engages only once the dial passes the sum of the other
  // stages' maxima — mirror that threshold from caps so the badge matches
  // the backend split without hardcoding 102.
  let ampStage = $derived(gainStages.find((g) => g.name === 'AMP') ?? null);
  let ampThreshold = $derived(
    ampStage
      ? gainStages
          .filter((g) => g.name !== 'AMP')
          .reduce((sum, g) => sum + (g.range_db?.max ?? 0), 0)
      : Number.POSITIVE_INFINITY,
  );
  let ampOn = $derived(manualGain ? (gainElementValues.AMP ?? 0) > 0 : currentGain > ampThreshold);
  let currentAntenna = $derived(
    typeof params.antenna === 'string' ? (params.antenna as string) : (antennas[0] ?? ''),
  );
  let currentAgc = $derived(params.agc === true);
  // Default-on: absent param means the driver tracker is engaged (the
  // SoapySource default). Only an explicit `false` turns it off.
  let currentDcOffset = $derived(params.dc_offset_correction !== false);

  // Software complex IQ DC blocker — auto-injected node-side after the
  // source on zero-IF radios (HackRF) to kill the LO/DC spike at the
  // tuned centre. It's a separate block (`dcblock`), not a source param,
  // so it's toggled via the generic block-param route. Absent block =>
  // not injected (non-hardware source) => hide the control.
  let dcBlock = $derived(pipeline.blocks.dcblock);
  let dcBlockValues = $derived((dcBlock?.values ?? {}) as Record<string, unknown>);
  let dcBlockEnabled = $derived(dcBlockValues.enabled !== false); // default on
  let dcBlockCorner = $derived(numberOr(dcBlockValues.corner_hz, 200));

  function numberOr(v: unknown, fallback: number): number {
    const n = typeof v === 'number' ? v : Number(v);
    return Number.isFinite(n) ? n : fallback;
  }

  async function commit(key: string, value: unknown) {
    pending = key;
    try {
      await applyControl(`flow.src.${key}`, value);
    } finally {
      pending = null;
    }
  }

  // A per-stage edit implies manual mode: send both atomically so the
  // server applies the stage value under `gain_mode=manual` in one delta.
  async function commitStage(name: string, value: number) {
    pending = `gain_elements.${name}`;
    try {
      await pipeline.patchSourceParams({
        gain_mode: 'manual',
        gain_elements: { ...gainElementValues, [name]: value },
      });
    } finally {
      pending = null;
    }
  }

  // Toggle / tune the injected `dcblock` (separate block, not `src`).
  async function commitDcBlock(key: string, value: unknown) {
    pending = `dcblock.${key}`;
    try {
      await applyControl(`flow.dcblock.${key}`, value);
    } finally {
      pending = null;
    }
  }

  async function commitSetting(s: SettingInfo, value: string) {
    const next = { ...settingsMap, [s.key]: value };
    // The `settings` sub-object is the driver's `writeSetting` bag —
    // merged server-side like any other source param, but not part of
    // the `apply_live_params` whitelist so it forces a rebuild.
    await commit('settings', next);
  }

  function settingValue(s: SettingInfo): string {
    return settingsMap[s.key] ?? s.default;
  }

  function fmtHz(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kHz`;
    return `${hz} Hz`;
  }

  function fmtRate(sps: number): string {
    if (sps >= 1e6) return `${(sps / 1e6).toFixed(3)} MS/s`;
    if (sps >= 1e3) return `${(sps / 1e3).toFixed(1)} kS/s`;
    return `${sps} S/s`;
  }

  function parseByType(t: SettingType, raw: string): string {
    if (t === 'bool') return raw === 'true' ? 'true' : 'false';
    return raw;
  }

  function rangeTooltip(r: RangeSpec | null | undefined, units: string): string {
    if (!r) return '';
    const step = r.step && r.step > 0 ? `, step ${r.step}` : ' (continuous)';
    return `range ${r.min}–${r.max} ${units}${step}`;
  }

  function settingTooltip(s: SettingInfo): string {
    const parts: string[] = [];
    const desc = effectiveDescription(s);
    if (desc) parts.push(desc);
    parts.push(`key: ${s.key}`);
    parts.push(`type: ${s.data_type}`);
    if (s.range) parts.push(rangeTooltip(s.range, s.units ?? ''));
    if (s.options.length > 0) parts.push(`${s.options.length} options`);
    if (s.default) parts.push(`default: ${s.default}`);
    return parts.join(' · ');
  }
</script>

<div class="flex flex-col gap-3 text-xs">
  {#if !caps}
    {#if pipeline.sourceCaps?.kind === 'software'}
      <p class="text-[10px] text-slate-600">
        {pipeline.sourceCaps.type_name}: no hardware controls.
      </p>
    {:else if pipeline.sourceCaps?.kind === 'unavailable'}
      <p class="text-[10px] text-rose-400">probe failed: {pipeline.sourceCaps.error}</p>
    {:else}
      <p class="text-[10px] text-slate-600">probing…</p>
    {/if}
  {/if}
  {#if caps && channel}
    <section class="flex flex-col gap-2">
      <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
        analog
      </header>
      {#if freqRange}
        <label class="row" title={rangeTooltip(freqRange, 'Hz')}>
          <span class="label">centre freq</span>
          <input
            type="number"
            step="0.001"
            min={freqRange.min / 1e6}
            max={freqRange.max / 1e6}
            value={Number.isFinite(currentFreqMHz) ? currentFreqMHz : ''}
            disabled={busy || pending !== null}
            onchange={(e) => {
              const mhz = Number((e.currentTarget as HTMLInputElement).value);
              if (!Number.isFinite(mhz)) return;
              commit('center_freq_hz', Math.round(mhz * 1e6));
            }}
          />
        </label>
      {/if}
      {#if rateChoices.length > 0}
        <label
          class="row"
          title={`curated list from preset; ${rateChoices.length} choices${
            caps ? ` · driver ${caps.driver_key}` : ''
          }`}
        >
          <span class="label">sample rate</span>
          <select
            value={String(currentRate)}
            disabled={busy || pending !== null}
            onchange={(e) =>
              commit('sample_rate_hz', Number((e.currentTarget as HTMLSelectElement).value))}
          >
            {#each rateChoices as r (r)}
              <option value={String(r)}>{fmtRate(r)}</option>
            {/each}
          </select>
        </label>
      {/if}
      {#if bwChoices.length > 0}
        <label
          class="row"
          title={`IF filter width; — auto — lets the driver pick. Preset ladder has ${bwChoices.length} entries.`}
        >
          <span class="label">bandwidth</span>
          <select
            value={Number.isFinite(currentBw) ? String(currentBw) : ''}
            disabled={busy || pending !== null}
            onchange={(e) => {
              const raw = (e.currentTarget as HTMLSelectElement).value;
              commit('bandwidth_hz', raw === '' ? null : Number(raw));
            }}
          >
            <option value="">— auto —</option>
            {#each bwChoices as b (b)}
              <option value={String(b)}>{fmtHz(b)}</option>
            {/each}
          </select>
        </label>
      {/if}
      {#if overallGainRange}
        <label class="row" title={gainDescription ?? rangeTooltip(overallGainRange, 'dB')}>
          <span class="label">{gainLabel}</span>
          <div class="range">
            <input
              type="range"
              min={overallGainRange.min}
              max={overallGainRange.max}
              step={overallGainRange.step ?? 1}
              value={gainDisplay}
              disabled={busy || pending !== null || currentAgc}
              oninput={(e) => {
                commit('gain_db', Number((e.currentTarget as HTMLInputElement).value));
              }}
            />
            <input
              type="number"
              min={overallGainRange.min}
              max={overallGainRange.max}
              step={overallGainRange.step ?? 1}
              value={gainDisplay}
              disabled={busy || pending !== null || currentAgc}
              onchange={(e) => {
                commit('gain_db', Number((e.currentTarget as HTMLInputElement).value));
              }}
            />
          </div>
        </label>
        {#if ampStage && !manualGain}
          <div
            class="row"
            title={`HackRF front-end +14 dB amp — engages above ${ampThreshold} dB on the auto dial. Overloads easily on strong signals.`}
          >
            <span class="label">AMP</span>
            <span class="amp-badge" class:on={ampOn}>{ampOn ? 'ON · +14 dB' : 'off'}</span>
          </div>
        {/if}
      {/if}
      {#if gainStages.length > 0}
        <details class="advanced-gain">
          <summary>Advanced gain (per stage)</summary>
          <label
            class="row"
            title="Manual = set each stage yourself; Auto = the single gain dial above."
          >
            <span class="label">Manual</span>
            <input
              type="checkbox"
              checked={manualGain}
              disabled={busy || pending !== null}
              onchange={(e) =>
                commit(
                  'gain_mode',
                  (e.currentTarget as HTMLInputElement).checked ? 'manual' : 'auto',
                )}
            />
          </label>
          {#each gainStages as stage (stage.name)}
            <label class="row" title={rangeTooltip(stage.range_db, 'dB')}>
              <span class="label">{stage.name}</span>
              <div class="range">
                <input
                  type="range"
                  min={stage.range_db.min}
                  max={stage.range_db.max}
                  step={stage.range_db.step ?? 1}
                  value={gainElementValues[stage.name] ?? stage.range_db.min}
                  disabled={busy || pending !== null || !manualGain}
                  oninput={(e) =>
                    commitStage(stage.name, Number((e.currentTarget as HTMLInputElement).value))}
                />
                <input
                  type="number"
                  min={stage.range_db.min}
                  max={stage.range_db.max}
                  step={stage.range_db.step ?? 1}
                  value={gainElementValues[stage.name] ?? stage.range_db.min}
                  disabled={busy || pending !== null || !manualGain}
                  onchange={(e) =>
                    commitStage(stage.name, Number((e.currentTarget as HTMLInputElement).value))}
                />
              </div>
            </label>
          {/each}
        </details>
      {/if}
      {#if hasAgc}
        <label class="row" title="set_gain_mode(true) — driver's internal AGC loop">
          <span class="label">AGC</span>
          <input
            type="checkbox"
            checked={currentAgc}
            disabled={busy || pending !== null}
            onchange={(e) => commit('agc', (e.currentTarget as HTMLInputElement).checked)}
          />
        </label>
      {/if}
      {#if hasDcOffset}
        <label
          class="row"
          title="set_dc_offset_mode — driver's automatic DC-offset (LO-leakage) tracking"
        >
          <span class="label">DC tracking</span>
          <input
            type="checkbox"
            checked={currentDcOffset}
            disabled={busy || pending !== null}
            onchange={(e) =>
              commit('dc_offset_correction', (e.currentTarget as HTMLInputElement).checked)}
          />
        </label>
      {/if}
      {#if dcBlock}
        <label
          class="row"
          title="Software complex IQ DC blocker — removes the zero-IF LO/DC spike at the tuned centre before the FFT split (gone from waterfall + narrow FFT + strongest-signal list). Toggle off to A/B whether a centre bin is a real carrier or the artifact."
        >
          <span class="label">DC blocker</span>
          <div class="range">
            <input
              type="checkbox"
              checked={dcBlockEnabled}
              disabled={busy || pending !== null}
              onchange={(e) =>
                commitDcBlock('enabled', (e.currentTarget as HTMLInputElement).checked)}
            />
            <input
              type="range"
              min="0"
              max="2000"
              step="10"
              value={dcBlockCorner}
              title="High-pass corner (Hz). ~200 nulls DC while leaving a carrier even 1 kHz off centre untouched. 0 = bypass."
              disabled={busy || pending !== null || !dcBlockEnabled}
              oninput={(e) =>
                commitDcBlock('corner_hz', Number((e.currentTarget as HTMLInputElement).value))}
            />
            <input
              type="number"
              min="0"
              max="2000"
              step="10"
              value={dcBlockCorner}
              title="High-pass corner (Hz). ~200 nulls DC while leaving a carrier even 1 kHz off centre untouched. 0 = bypass."
              disabled={busy || pending !== null || !dcBlockEnabled}
              onchange={(e) =>
                commitDcBlock('corner_hz', Number((e.currentTarget as HTMLInputElement).value))}
            />
            <span class="unit">Hz</span>
          </div>
        </label>
      {/if}
      {#if antennas.length > 1}
        <label class="row" title={`RF port · ${antennas.length} options`}>
          <span class="label">antenna</span>
          <select
            value={currentAntenna}
            disabled={busy || pending !== null}
            onchange={(e) => commit('antenna', (e.currentTarget as HTMLSelectElement).value)}
          >
            {#each antennas as a (a)}
              <option value={a}>{a}</option>
            {/each}
          </select>
        </label>
      {/if}
    </section>
  {/if}

  {#if visibleSettings.length > 0}
    <section class="flex flex-col gap-2 border-t border-slate-800 pt-3">
      <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
        driver settings
      </header>
      {#each visibleSettings as s (s.key)}
        {@const value = settingValue(s)}
        <label class="row" title={settingTooltip(s)}>
          <span class="label">{effectiveLabel(s)}{s.units ? ` (${s.units})` : ''}</span>

          {#if s.data_type === 'bool'}
            <input
              type="checkbox"
              checked={value === 'true'}
              disabled={busy || pending !== null}
              onchange={(e) =>
                commitSetting(s, (e.currentTarget as HTMLInputElement).checked ? 'true' : 'false')}
            />
          {:else if s.options.length > 0}
            <select
              {value}
              disabled={busy || pending !== null}
              onchange={(e) => commitSetting(s, (e.currentTarget as HTMLSelectElement).value)}
            >
              {#each s.options as opt (opt.value)}
                <option value={opt.value}>{effectiveOptionLabel(s, opt.value, opt.label)}</option>
              {/each}
            </select>
          {:else if s.range && (s.data_type === 'int' || s.data_type === 'float')}
            <div class="range">
              <input
                type="range"
                min={s.range.min}
                max={s.range.max}
                step={s.range.step ?? (s.data_type === 'int' ? 1 : 'any')}
                {value}
                disabled={busy || pending !== null}
                oninput={(e) => commitSetting(s, (e.currentTarget as HTMLInputElement).value)}
              />
              <input
                type="number"
                min={s.range.min}
                max={s.range.max}
                step={s.range.step ?? (s.data_type === 'int' ? 1 : 'any')}
                {value}
                disabled={busy || pending !== null}
                onchange={(e) => commitSetting(s, (e.currentTarget as HTMLInputElement).value)}
              />
            </div>
          {:else}
            <input
              type={s.data_type === 'int' || s.data_type === 'float' ? 'number' : 'text'}
              {value}
              disabled={busy || pending !== null}
              onchange={(e) =>
                commitSetting(
                  s,
                  parseByType(s.data_type, (e.currentTarget as HTMLInputElement).value),
                )}
            />
          {/if}
        </label>
      {/each}
    </section>
  {/if}

  {#if caps && !channel && caps.settings.length === 0}
    <p class="text-[10px] text-slate-600">no controls advertised by this driver</p>
  {/if}
</div>

<style>
  .row {
    display: grid;
    grid-template-columns: 8rem 1fr;
    align-items: center;
    gap: 0.5rem;
  }
  .label {
    color: var(--color-muted);
    user-select: none;
  }
  select,
  input[type='text'],
  input[type='number'] {
    background: rgb(15 23 42);
    border: 1px solid rgb(30 41 59);
    border-radius: 0.25rem;
    padding: 0.15rem 0.4rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
  }
  .range {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .range input[type='range'] {
    flex: 1 1 auto;
    min-width: 5rem;
  }
  .range input[type='number'] {
    width: 5rem;
  }
  .amp-badge {
    font-size: 0.7rem;
    padding: 0.05rem 0.4rem;
    border-radius: 0.25rem;
    background: #333;
    color: #999;
    user-select: none;
  }
  .amp-badge.on {
    background: #7a3b00;
    color: #ffb066;
  }
  .advanced-gain {
    margin: 0.2rem 0 0.1rem;
  }
  .advanced-gain summary {
    cursor: pointer;
    color: #aaa;
    font-size: 0.75rem;
    user-select: none;
  }
  .unit {
    color: #888;
    font-size: 0.75rem;
  }
</style>
