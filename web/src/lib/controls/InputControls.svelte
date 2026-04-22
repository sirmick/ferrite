<script lang="ts">
  // Input panel for the active SDR — driver-specific knobs surfaced via
  // `getSettingInfo` plus the restart-on-change analog basics (sample
  // rate, bandwidth). The hot/live controls (gain, antenna, AGC, freq)
  // live in the spectrum header by the nixies (#70), not here.
  //
  // Reads `pipeline.sourceCaps` for the schema and `pipeline.source.params`
  // for current values; every change calls `pipeline.patchSourceParams`,
  // which the server applies by tearing down + rebuilding the source.

  import { pipeline } from '$lib/pipeline.svelte';
  import type { SettingInfo, SettingType } from '$lib/api/devices';
  import { rangesToChoices } from './optionsModel';

  // Reads capabilities + current params straight off the pipeline store
  // — same pattern as `BlockParams`. Keeps the parent panel a one-liner.
  let caps = $derived(
    pipeline.sourceCaps?.kind === 'hardware' ? pipeline.sourceCaps.capabilities : null,
  );
  let params = $derived((pipeline.source?.params ?? {}) as Record<string, unknown>);
  let pending = $state<string | null>(null);
  let busy = $derived(pipeline.phase === 'busy');

  let settingsMap = $derived((params.settings ?? {}) as Record<string, string>);
  let channel = $derived(caps?.rx_channels[0] ?? null);

  let rateChoices = $derived(channel ? rangesToChoices(channel.sample_rate_ranges_hz) : []);
  let bwChoices = $derived(channel ? rangesToChoices(channel.bandwidth_ranges_hz) : []);

  let currentRate = $derived(numberOr(params.sample_rate_hz, rateChoices[0] ?? NaN));
  let currentBw = $derived(numberOr(params.bandwidth_hz, NaN));

  function numberOr(v: unknown, fallback: number): number {
    const n = typeof v === 'number' ? v : Number(v);
    return Number.isFinite(n) ? n : fallback;
  }

  async function commit(label: string, patch: Record<string, unknown>) {
    pending = label;
    try {
      await pipeline.patchSourceParams(patch);
    } finally {
      pending = null;
    }
  }

  async function commitSetting(s: SettingInfo, value: string) {
    const next = { ...settingsMap, [s.key]: value };
    await commit(s.key, { settings: next });
  }

  function settingValue(s: SettingInfo): string {
    return settingsMap[s.key] ?? s.default;
  }

  function fmtHz(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kHz`;
    return `${hz} Hz`;
  }

  // Sample rate is samples-per-second, not Hz. Same magnitudes, different
  // unit symbol — keep them visually distinct in the dropdowns.
  function fmtRate(sps: number): string {
    if (sps >= 1e6) return `${(sps / 1e6).toFixed(3)} MS/s`;
    if (sps >= 1e3) return `${(sps / 1e3).toFixed(1)} kS/s`;
    return `${sps} S/s`;
  }

  function parseByType(t: SettingType, raw: string): string {
    // Soapy is permissive on the wire — everything is a string. We
    // validate-but-don't-coerce so the round trip stays loss-free.
    if (t === 'bool') return raw === 'true' ? 'true' : 'false';
    return raw;
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
      {#if rateChoices.length > 0}
        <label class="row">
          <span class="label">sample rate</span>
          <select
            value={String(currentRate)}
            disabled={busy || pending !== null}
            onchange={(e) =>
              commit('sample_rate_hz', {
                sample_rate_hz: Number((e.currentTarget as HTMLSelectElement).value),
              })}
          >
            {#each rateChoices as r (r)}
              <option value={String(r)}>{fmtRate(r)}</option>
            {/each}
          </select>
        </label>
      {/if}
      {#if bwChoices.length > 0}
        <label class="row">
          <span class="label">bandwidth</span>
          <select
            value={Number.isFinite(currentBw) ? String(currentBw) : ''}
            disabled={busy || pending !== null}
            onchange={(e) => {
              const raw = (e.currentTarget as HTMLSelectElement).value;
              commit('bandwidth_hz', { bandwidth_hz: raw === '' ? null : Number(raw) });
            }}
          >
            <option value="">— auto —</option>
            {#each bwChoices as b (b)}
              <option value={String(b)}>{fmtHz(b)}</option>
            {/each}
          </select>
        </label>
      {/if}
    </section>
  {/if}

  {#if caps && caps.settings.length > 0}
    <section class="flex flex-col gap-2 border-t border-slate-800 pt-3">
      <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
        driver settings
      </header>
      {#each caps.settings as s (s.key)}
        {@const value = settingValue(s)}
        <label class="row" title={s.description ?? s.key}>
          <span class="label">{s.label}{s.units ? ` (${s.units})` : ''}</span>

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
                <option value={opt.value}>{opt.label ?? opt.value}</option>
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
</style>
