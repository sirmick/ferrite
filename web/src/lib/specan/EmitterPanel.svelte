<script lang="ts">
  // The spectrum analyzer's optional right panel: the station's emitters
  // inside the swept range — the same table (number, frequency, level,
  // width, band / label) as the receiver's Strongest Signals pane. Rows not
  // seen recently are dimmed. Click = marker, double-click = listen there.
  import EmitterTable, { type EmitterRow } from '$lib/emitters/EmitterTable.svelte';
  import { emitters } from '$lib/emitters/store.svelte';
  import SnrCutoff from '$lib/emitters/SnrCutoff.svelte';
  import { pipeline } from '$lib/pipeline.svelte';
  import { specan } from './specan.svelte';

  type Filter = 'all' | 'new' | 'identified' | 'ignored';
  let filter = $state<Filter>('all');
  let sortBy = $state<'freq' | 'level'>('freq');

  let lo = $derived(specan.range[0]);
  let hi = $derived(specan.range[1]);
  // "Recent" = within a few passes (or a minute for a fast sweep).
  let staleMs = $derived(Math.max(60_000, (specan.passSeconds ?? 10) * 4_000));
  let inRange = $derived(emitters.inRange(lo, hi, true));
  let pruneNote = $state('');
  let inspecting = $state<number | null>(null);
  let inspectNote = $state('');

  async function inspect(r: EmitterRow) {
    if (r.id === undefined) return;
    inspecting = r.id;
    inspectNote = '';
    try {
      await emitters.inspect(r.id, true);
    } catch (e) {
      inspectNote = String(e);
    } finally {
      inspecting = null;
    }
  }

  async function prune() {
    if (
      !confirm(
        `Delete unlabelled emitters below ${emitters.config.min_snr_db} dB SNR, seen-once ones and long-unseen ones? Labelled and ignored emitters are kept.`,
      )
    )
      return;
    try {
      const r = await emitters.prune();
      pruneNote = `${r.removed} removed`;
    } catch (e) {
      pruneNote = String(e);
    }
  }
  let counts = $derived({
    all: inRange.filter((e) => e.status !== 'ignored').length,
    new: inRange.filter((e) => e.status === 'new').length,
    identified: inRange.filter((e) => e.status === 'identified').length,
    ignored: inRange.filter((e) => e.status === 'ignored').length,
  });
  let rows = $derived.by<EmitterRow[]>(() => {
    const now = Date.now();
    const list = inRange
      .filter((e) => (filter === 'all' ? e.status !== 'ignored' : e.status === filter))
      .map((e) => ({
        key: e.id,
        id: e.id,
        freq_hz: e.freq_hz,
        width_hz: e.width_hz,
        level_dbfs: e.level_dbfs,
        snr_db: e.snr_db,
        band: e.band,
        label: e.label,
        status: e.status,
        last_seen_ms: e.last_seen_ms,
        stale: now - e.last_seen_ms > staleMs,
        hint: e.inspect?.hint,
      }));
    return sortBy === 'freq'
      ? list.sort((a, b) => a.freq_hz - b.freq_hz)
      : list.sort((a, b) => b.level_dbfs - a.level_dbfs);
  });

  async function listen(r: EmitterRow) {
    await pipeline.returnPreset();
    await pipeline.tune(r.freq_hz);
  }
</script>

<div
  class="flex h-full w-full min-h-0 flex-col border-l border-slate-800 bg-[color:var(--color-bg)]"
>
  <header class="panel-head flex flex-col items-stretch gap-1">
    <div class="flex items-center gap-2">
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">Emitters</span>
      <div class="flex overflow-hidden rounded border border-slate-700 text-[10px]">
        {#each ['all', 'new', 'identified', 'ignored'] as const as f (f)}
          <button
            type="button"
            class="whitespace-nowrap px-1.5 py-0.5 hover:bg-slate-800"
            class:f-on={filter === f}
            onclick={() => (filter = f)}>{f} {counts[f]}</button
          >
        {/each}
      </div>
    </div>
    <div class="flex items-center gap-3">
      <SnrCutoff />
      <button
        type="button"
        class="text-[10px] text-[color:var(--color-muted)] hover:text-slate-200"
        title={pruneNote ||
          'Delete unlabelled emitters below the SNR cutoff, seen-once and long-unseen ones (labelled / ignored are kept)'}
        onclick={() => void prune()}>prune{pruneNote ? ` · ${pruneNote}` : ''}</button
      >
      <button
        type="button"
        class="ml-auto whitespace-nowrap text-[10px] text-[color:var(--color-muted)] hover:text-slate-200"
        title="Sort"
        onclick={() => (sortBy = sortBy === 'freq' ? 'level' : 'freq')}>by {sortBy}</button
      >
    </div>
  </header>
  {#if inspecting !== null || inspectNote}
    <div class="border-b border-slate-800 px-2 py-0.5 text-[10px] text-[color:var(--color-muted)]">
      {inspecting !== null
        ? `inspecting #${inspecting} — the receiver takes the radio for a few seconds, then the sweep resumes`
        : inspectNote}
    </div>
  {/if}
  <div class="min-h-0 flex-1 overflow-auto">
    <EmitterTable
      {rows}
      activeHz={specan.marker}
      onPick={(r) => (specan.marker = r.freq_hz)}
      onListen={(r) => void listen(r)}
      onInspect={(r) => void inspect(r)}
      busy={inspecting}
      empty={filter === 'all'
        ? 'nothing above the floor in this range yet — let a pass finish'
        : `no ${filter} emitters in this range`}
    />
  </div>
</div>

<style>
  .f-on {
    background: rgba(125, 211, 252, 0.15);
    color: #e2e8f0;
  }
</style>
