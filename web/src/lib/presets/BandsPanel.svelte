<script lang="ts">
  import { bands, type BandEntry } from './bands';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';

  let openGroups = $state<Record<string, boolean>>({});
  function toggle(name: string) {
    openGroups[name] = !(openGroups[name] ?? defaultOpen(name));
  }
  function defaultOpen(name: string): boolean {
    return bands[0]?.name === name;
  }

  function fmtHz(hz: number): string {
    if (hz >= 1e9) return `${(hz / 1e9).toFixed(3)} GHz`;
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(3)} kHz`;
    return `${hz} Hz`;
  }

  let tuning = $state(false);
  async function tune(e: BandEntry) {
    if (!pipeline.source) return;
    // In-flight guard — preset swaps on SDR hardware can take multiple
    // seconds (device teardown + reopen), and a second click during
    // that window would stack another concurrent tune. Drop the new
    // click on the floor; the button also goes `disabled` so the user
    // gets visual feedback that the first click is still running.
    if (tuning) return;
    tuning = true;
    try {
      // Swap presets first when the entry pins one and it differs from
      // the currently-loaded doc. Tuning happens after so the new
      // preset's source block gets the target center_freq.
      if (e.preset && pipeline.flowgraph?.name !== e.preset) {
        const resp = await pipeline.loadPreset(e.preset);
        if (resp === null) return;
      }
      await applyControl('flow.src.center_freq_hz', e.hz);
    } finally {
      tuning = false;
    }
  }

  let axes = $derived(currentAxes(pipeline));
  let active = $derived(axes?.center_freq_hz ?? null);
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between gap-2 border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Bands</span>
    <div class="flex items-center gap-2">
      <a
        href="https://www.ntia.gov/sites/default/files/2025-09/ntia-us-frequency-allocations.pdf"
        target="_blank"
        rel="noopener noreferrer"
        class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-tight text-[color:var(--color-muted)] hover:border-slate-500 hover:text-slate-200"
        title="NTIA US Frequency Allocations chart (PDF, Sep 2025)"
      >
        US chart ↗
      </a>
      <a
        href="https://www.sigidwiki.com/wiki/Signal_Identification_Guide"
        target="_blank"
        rel="noopener noreferrer"
        class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-tight text-[color:var(--color-muted)] hover:border-slate-500 hover:text-slate-200"
        title="sigidwiki — Signal Identification Guide"
      >
        Signal Wiki ↗
      </a>
      <span class="text-[10px] text-[color:var(--color-muted)]">click to tune</span>
    </div>
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#each bands as group (group.name)}
      {@const isOpen = openGroups[group.name] ?? defaultOpen(group.name)}
      <div class="border-b border-slate-900">
        <button
          type="button"
          class="flex w-full items-center justify-between px-2 py-1 text-left hover:bg-slate-900/60"
          onclick={() => toggle(group.name)}
        >
          <span class="text-[color:var(--color-muted)]">{group.name}</span>
          <span class="text-[10px] text-[color:var(--color-muted)]">{isOpen ? '▾' : '▸'}</span>
        </button>
        {#if isOpen}
          <ul class="pb-1">
            {#each group.entries as entry (entry.hz + entry.label)}
              <li>
                <button
                  type="button"
                  class="flex w-full items-center justify-between gap-2 px-3 py-0.5 text-left text-[11px] hover:bg-slate-800/70 disabled:cursor-wait disabled:opacity-60"
                  class:active={active === entry.hz}
                  disabled={!pipeline.source || tuning}
                  onclick={() => void tune(entry)}
                >
                  <span class="truncate">{entry.label}</span>
                  <span class="shrink-0 font-mono text-[10px] text-slate-400">
                    {fmtHz(entry.hz)}
                    {#if entry.mode}
                      <span class="ml-1 text-[9px] text-slate-500">{entry.mode}</span>
                    {/if}
                  </span>
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .active {
    background: rgba(125, 211, 252, 0.15);
    color: #7dd3fc;
  }
</style>
