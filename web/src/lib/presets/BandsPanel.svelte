<script lang="ts">
  import { bands, type BandEntry } from './bands';
  import { pipeline } from '$lib/pipeline.svelte';
  import { presetSlugForMode } from './receivers';
  import { presetDocBySlug } from './catalog';

  // Receiver presets that actually shipped in this build. `+RX` is
  // only offered when the mode's canonical preset is installed —
  // otherwise the button would 404 at load time.
  const installedSlugs = new Set(presetDocBySlug.keys());
  function rxSlug(mode: string | undefined): string | null {
    const slug = presetSlugForMode(mode);
    return slug && installedSlugs.has(slug) ? slug : null;
  }

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

  // Selection is tracked by a stable per-entry key, NOT by frequency.
  // Several entries legitimately share a frequency (e.g. RTTY/PSK31
  // both 10.142 MHz) — keying the highlight on `center_freq_hz` lit up
  // every one of them at once. The key is group-scoped + positional so
  // duplicates stay distinct.
  function entryKey(groupName: string, idx: number): string {
    return `${groupName}#${idx}`;
  }
  let selectedKey = $state<string | null>(null);

  // One in-flight guard for the whole panel: back-to-back patches race,
  // so any pending tune/RX disables every action button until it lands.
  let busy = $state(false);

  // Route through `pipeline.tune` so the per-driver DC-spike dodge
  // (HackRF & friends) is applied uniformly with the VFO/Nixie/▲▼ paths
  // — the daemon owns the per-driver dodge ratio and picks src_center vs
  // chan.freq_shift_hz from it. The legacy `e.vfo_offset_hz` (a hand-tuned
  // per-band BFO/dodge nudge from before the central dodge existed) is
  // intentionally ignored; if a band entry needs a non-default shift,
  // model it on the demod side, not as a tune offset.
  async function tuneFreq(e: BandEntry): Promise<void> {
    await pipeline.tune(e.hz);
  }

  // Load the canonical receiver preset for this entry's mode — a
  // coherent end-to-end chain (channelizer width, demod, audio rates),
  // exactly like picking it from the Signal Catalog. The server
  // preserves the live centre frequency across the swap; the caller
  // re-tunes to the band entry afterwards. No-op return when the mode
  // has no installed receiver preset.
  async function setReceiver(e: BandEntry): Promise<boolean> {
    const slug = rxSlug(e.mode);
    if (!slug) return false;
    await pipeline.loadPreset(slug);
    return true;
  }

  async function onTune(key: string, e: BandEntry): Promise<void> {
    if (busy || !pipeline.source) return;
    busy = true;
    try {
      await tuneFreq(e);
      selectedKey = key;
    } finally {
      busy = false;
    }
  }

  async function onTuneRx(key: string, e: BandEntry): Promise<void> {
    if (busy || !pipeline.source) return;
    busy = true;
    try {
      // Preset load first, tune second: loadPreset swaps the whole
      // chain (and the server re-merges live source params over the
      // new preset's hints), so the explicit tuneFreq() afterwards is
      // what lands us on the band entry's actual frequency + VFO.
      await setReceiver(e);
      await tuneFreq(e);
      selectedKey = key;
    } finally {
      busy = false;
    }
  }
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
            {#each group.entries as entry, idx (entryKey(group.name, idx))}
              {@const key = entryKey(group.name, idx)}
              {@const rxId = rxSlug(entry.mode)}
              <li
                class="flex items-center justify-between gap-2 px-3 py-0.5 text-[11px]"
                class:active={selectedKey === key}
              >
                <span class="min-w-0 flex-1 truncate" title={entry.label}>{entry.label}</span>
                <span class="shrink-0 font-mono text-[10px] text-slate-400">
                  {fmtHz(entry.hz)}
                  {#if entry.mode}
                    <span class="ml-1 text-[9px] text-slate-500">{entry.mode}</span>
                  {/if}
                </span>
                <span class="flex shrink-0 gap-1">
                  <button
                    type="button"
                    class="rounded border border-slate-700 px-1.5 leading-tight text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:cursor-wait disabled:opacity-50"
                    disabled={!pipeline.source || busy}
                    title="Tune the SDR to this frequency (demod chain unchanged)"
                    onclick={() => void onTune(key, entry)}
                  >
                    Tune
                  </button>
                  <button
                    type="button"
                    class="rounded border border-sky-800 px-1.5 leading-tight text-sky-300 hover:border-sky-500 hover:text-sky-100 disabled:cursor-not-allowed disabled:border-slate-800 disabled:text-slate-600 disabled:opacity-60"
                    disabled={!pipeline.source || busy || !rxId}
                    title={rxId
                      ? `Tune here and load the ${rxId} receiver preset`
                      : `No receiver preset for ${entry.mode ?? 'this mode'} — use Tune`}
                    onclick={() => void onTuneRx(key, entry)}
                  >
                    +RX
                  </button>
                </span>
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
