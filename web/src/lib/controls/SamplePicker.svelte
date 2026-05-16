<script lang="ts">
  import { fetchCaptures, type CaptureEntry } from '$lib/api/captures';
  import type { SourceConfig } from '$lib/api/source';

  interface Props {
    /** Fired when the user applies a sample, parent PATCHes source. */
    onApply: (cfg: SourceConfig) => void;
    onCancel: () => void;
  }

  let { onApply, onCancel }: Props = $props();

  type LoadState =
    | { kind: 'loading' }
    | { kind: 'error'; message: string }
    | { kind: 'ok'; entries: CaptureEntry[] };

  type Mod = 'am' | 'fm' | 'ssb';
  type Sb = 'usb' | 'lsb';
  type Kind = 'audio' | 'iq';
  const MODS: Mod[] = ['am', 'fm', 'ssb'];
  const SBS: Sb[] = ['usb', 'lsb'];
  const KINDS: Kind[] = ['audio', 'iq'];

  let load = $state<LoadState>({ kind: 'loading' });
  let selected = $state<CaptureEntry | null>(null);
  let custom = $state(false);

  // Replay controls (seeded from the picked sample's sidecar).
  let customPath = $state('');
  let customKind = $state<Kind>('audio');
  let modulation = $state<Mod>('ssb');
  let sideband = $state<Sb>('usb');
  let offsetKHz = $state(12);
  let deviationKHz = $state(5);
  let modDepth = $state(0.7);
  let outRateHz = $state(48_000);
  let loop = $state(false);

  async function refresh() {
    load = { kind: 'loading' };
    try {
      load = { kind: 'ok', entries: await fetchCaptures() };
    } catch (err) {
      load = { kind: 'error', message: err instanceof Error ? err.message : String(err) };
    }
  }

  $effect(() => {
    void refresh();
  });

  function pick(e: CaptureEntry) {
    selected = e;
    custom = false;
    const k = e.kind;
    customKind = k;
    const m = (e.modulation ?? '').toLowerCase();
    modulation = m === 'am' || m === 'fm' ? m : 'ssb';
    // IQ replays straight (shift 0); audio rides a carrier in-band.
    offsetKHz = k === 'iq' ? 0 : 12;
    // IqUpmix is 1:1 - keep the capture rate; audio needs an IQ rate
    // wide enough for the carrier + skirts.
    const sr = e.sample_rate_hz ?? 48_000;
    outRateHz = k === 'iq' ? Math.round(sr) : Math.max(48_000, Math.round(sr) * 4);
  }

  function startCustom() {
    custom = true;
    selected = null;
  }

  const label = (e: CaptureEntry) => e.name ?? e.rel;
  const fmtHz = (n: number | null) =>
    n == null ? '' : n >= 1e6 ? `${(n / 1e6).toFixed(3)} MHz` : `${(n / 1e3).toFixed(3)} kHz`;

  let ready = $derived(custom ? customPath.trim().length > 0 : selected !== null);
  let activeKind = $derived(custom ? customKind : (selected?.kind ?? 'audio'));

  function apply() {
    const path = custom ? customPath.trim() : (selected?.path ?? '');
    if (!path) return;
    const sr = custom ? 0 : (selected?.sample_rate_hz ?? 0);
    onApply({
      type: 'ModulatedFileSource',
      params: {
        path,
        kind: activeKind,
        modulation,
        sideband,
        offset_hz: offsetKHz * 1000,
        deviation_hz: deviationKHz * 1000,
        mod_depth: modDepth,
        output_rate_hz: outRateHz,
        rate_hz_hint: sr,
        center_freq_hz: (custom ? 0 : selected?.center_freq_hz) ?? 0,
        loop_playback: loop,
      },
    });
  }
</script>

<div class="flex flex-col gap-3 text-sm">
  <div class="flex items-center justify-between">
    <h2 class="text-sm font-semibold">Samples</h2>
    <button
      type="button"
      class="rounded border border-slate-700 px-2 py-0.5 text-xs hover:border-slate-600"
      onclick={() => void refresh()}
      disabled={load.kind === 'loading'}
    >
      {load.kind === 'loading' ? 'Loading...' : 'Refresh'}
    </button>
  </div>

  {#if load.kind === 'loading'}
    <p class="text-xs text-[color:var(--color-muted)]">Scanning samples/...</p>
  {:else if load.kind === 'error'}
    <p class="text-xs text-rose-400">Failed to list samples: {load.message}</p>
  {:else if load.entries.length === 0}
    <p class="text-xs text-[color:var(--color-muted)]">
      No samples found under the server's captures dir.
    </p>
  {:else}
    <ul class="flex flex-col gap-1.5">
      {#each load.entries as e (e.path)}
        <li>
          <button
            type="button"
            class="flex w-full flex-col gap-1 rounded border bg-slate-900/40 p-2.5 text-left transition-colors hover:border-[color:var(--color-accent)]"
            class:border-slate-800={selected?.path !== e.path}
            class:sel={selected?.path === e.path}
            onclick={() => pick(e)}
          >
            <div class="flex items-baseline justify-between gap-2">
              <span class="font-medium">{label(e)}</span>
              <span
                class="rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide"
                class:iq-badge={e.kind === 'iq'}
                class:audio-badge={e.kind === 'audio'}
              >
                {e.kind}
              </span>
            </div>
            <span class="font-mono text-[10px] text-[color:var(--color-muted)]">{e.rel}</span>
            <span class="text-[10px] text-[color:var(--color-muted)]">
              {[
                e.sample_rate_hz ? fmtHz(e.sample_rate_hz) : null,
                e.center_freq_hz ? `@ ${fmtHz(e.center_freq_hz)}` : null,
                e.kind === 'audio' && e.modulation ? `${e.modulation} carrier` : null,
              ]
                .filter(Boolean)
                .join(' | ')}
            </span>
          </button>
        </li>
      {/each}
    </ul>
    <button
      type="button"
      class="self-start text-[11px] text-[color:var(--color-muted)] underline hover:text-[color:var(--color-fg)]"
      onclick={startCustom}
    >
      > Advanced: open a custom server path
    </button>
  {/if}

  {#if custom || selected}
    <div class="flex flex-col gap-3 rounded border border-slate-800 bg-slate-900/30 p-3">
      <div class="text-xs font-semibold">
        {#if custom}
          Custom server path
        {:else}
          Selected: {selected ? label(selected) : ''}
        {/if}
      </div>

      {#if custom}
        <label class="grid gap-1 text-xs">
          <span class="text-[color:var(--color-muted)]">Server file path</span>
          <input
            type="text"
            bind:value={customPath}
            placeholder="/abs/path/to/capture.cf32 or .wav"
            class="rounded border border-slate-800 bg-slate-900 px-2 py-1 font-mono"
          />
        </label>
        <div class="flex gap-1 text-xs">
          {#each KINDS as k (k)}
            <button
              type="button"
              class="rounded border px-2 py-1"
              class:seg-on={customKind === k}
              class:seg-off={customKind !== k}
              onclick={() => (customKind = k)}
            >
              {k === 'iq' ? 'IQ (upmix)' : 'Audio (modulate)'}
            </button>
          {/each}
        </div>
      {/if}

      {#if activeKind === 'audio'}
        <div class="grid gap-1 text-xs">
          <span class="text-[color:var(--color-muted)]">Replay as - modulated audio</span>
          <div class="flex gap-1">
            {#each MODS as m (m)}
              <button
                type="button"
                class="rounded border px-3 py-1 uppercase"
                class:seg-on={modulation === m}
                class:seg-off={modulation !== m}
                onclick={() => (modulation = m)}
              >
                {m}
              </button>
            {/each}
            {#if modulation === 'ssb'}
              <div class="ml-auto flex gap-1">
                {#each SBS as s (s)}
                  <button
                    type="button"
                    class="rounded border px-2 py-1 uppercase"
                    class:seg-on={sideband === s}
                    class:seg-off={sideband !== s}
                    onclick={() => (sideband = s)}
                  >
                    {s}
                  </button>
                {/each}
              </div>
            {/if}
          </div>
        </div>

        <label class="grid gap-1 text-xs">
          <div class="flex justify-between">
            <span class="text-[color:var(--color-muted)]">Carrier offset</span>
            <span class="font-mono">{offsetKHz >= 0 ? '+' : '-'}{Math.abs(offsetKHz)} kHz</span>
          </div>
          <input type="range" min="-40" max="40" step="0.5" bind:value={offsetKHz} />
        </label>

        {#if modulation === 'fm'}
          <label class="grid gap-1 text-xs">
            <div class="flex justify-between">
              <span class="text-[color:var(--color-muted)]">Peak deviation</span>
              <span class="font-mono">{deviationKHz} kHz</span>
            </div>
            <input type="range" min="1" max="75" step="1" bind:value={deviationKHz} />
          </label>
        {/if}
        {#if modulation === 'am'}
          <label class="grid gap-1 text-xs">
            <div class="flex justify-between">
              <span class="text-[color:var(--color-muted)]">Modulation depth</span>
              <span class="font-mono">{modDepth.toFixed(2)}</span>
            </div>
            <input type="range" min="0.1" max="1" step="0.05" bind:value={modDepth} />
          </label>
        {/if}
      {:else}
        <label class="grid gap-1 text-xs">
          <div class="flex justify-between">
            <span class="text-[color:var(--color-muted)]">Frequency shift</span>
            <span class="font-mono">
              {offsetKHz === 0
                ? 'straight replay'
                : `${offsetKHz > 0 ? '+' : '-'}${Math.abs(offsetKHz)} kHz`}
            </span>
          </div>
          <input type="range" min="-40" max="40" step="0.5" bind:value={offsetKHz} />
        </label>
      {/if}

      <label class="grid gap-1 text-xs">
        <div class="flex justify-between">
          <span class="text-[color:var(--color-muted)]">Output IQ rate</span>
          <span class="font-mono">{(outRateHz / 1000).toFixed(1)} kHz</span>
        </div>
        <input
          type="number"
          step="1000"
          min="8000"
          bind:value={outRateHz}
          class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
        />
      </label>

      <label class="flex items-center gap-2 text-xs">
        <input type="checkbox" bind:checked={loop} />
        <span>Loop playback</span>
      </label>
    </div>
  {/if}

  <div class="flex justify-end gap-2 pt-1">
    <button
      type="button"
      class="rounded border border-slate-700 px-3 py-1 text-sm"
      onclick={onCancel}
    >
      Cancel
    </button>
    <button
      type="button"
      class="rounded bg-[color:var(--color-accent)] px-3 py-1 text-sm font-semibold text-slate-900 disabled:opacity-40"
      disabled={!ready}
      onclick={apply}
    >
      Apply
    </button>
  </div>
</div>

<style>
  .sel {
    border-color: var(--color-accent);
    background: color-mix(in srgb, var(--color-accent) 12%, transparent);
  }
  .iq-badge {
    background: color-mix(in srgb, var(--color-accent) 22%, transparent);
    color: var(--color-accent);
  }
  .audio-badge {
    background: rgb(51 65 85 / 0.7);
    color: rgb(203 213 225);
  }
  .seg-on {
    border-color: var(--color-accent);
    color: var(--color-fg);
    font-weight: 600;
  }
  .seg-off {
    border-color: rgb(30 41 59);
    color: var(--color-muted);
  }
</style>
