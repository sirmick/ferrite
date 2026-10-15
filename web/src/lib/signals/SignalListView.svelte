<script lang="ts">
  // Strongest-signal list — occupies the right (channel-detail) column
  // in place of the narrow FFT/waterfall when the operator picks
  // Display → Right → "Strongest Signals" (offered when the active preset
  // wires a SignalList at the wideband FFT). Rows are the auto-detected
  // signals across the whole span, ranked strongest-first, joined to the
  // station's emitter list (same number / band / label as the spectrum
  // analyzer's panel — one EmitterTable for both). Ignored emitters
  // (spurs) are hidden. Click or double-click tunes there.

  import { currentAxes, pipeline } from '$lib/pipeline.svelte';
  import { signals } from '$lib/signals/store.svelte';
  import { emitters } from '$lib/emitters/store.svelte';
  import EmitterTable, { type EmitterRow } from '$lib/emitters/EmitterTable.svelte';
  import BlockParams from '$lib/controls/BlockParams.svelte';

  // The node-side SignalList detector backing this panel has live Min SNR
  // (detection threshold) + Top N knobs.
  let signalsBlock = $derived(pipeline.blocks['signals']);

  // One row per emitter: the live detections (a station can be a dozen
  // fragments) collapse onto the emitter they belong to — strongest
  // detection's frequency and level. Detections not (yet) tied to an
  // emitter show as themselves.
  let joined = $derived.by<EmitterRow[]>(() => {
    const byEmitter = new Map<number, EmitterRow>();
    const loose: EmitterRow[] = [];
    for (const s of signals.signals) {
      const e = emitters.match(s.freq_hz, s.bw_hz, s.power_db);
      if (!e) {
        loose.push({
          key: `s${s.id}`,
          freq_hz: s.freq_hz,
          width_hz: s.bw_hz,
          level_dbfs: s.power_db,
          snr_db: s.snr_db,
        });
        continue;
      }
      const prev = byEmitter.get(e.id);
      if (prev && prev.level_dbfs >= s.power_db) continue;
      byEmitter.set(e.id, {
        key: `e${e.id}`,
        id: e.id,
        freq_hz: s.freq_hz,
        width_hz: Math.max(e.width_hz, s.bw_hz),
        level_dbfs: s.power_db,
        snr_db: Math.max(s.snr_db, prev?.snr_db ?? -Infinity),
        band: e.band,
        label: e.label,
        status: e.status,
        last_seen_ms: e.last_seen_ms,
      });
    }
    return [...byEmitter.values(), ...loose].sort((a, b) => b.level_dbfs - a.level_dbfs);
  });
  let rows = $derived(joined.filter((r) => r.status !== 'ignored'));
  let hidden = $derived(joined.length - rows.length);
  // Where the VFO listens: LO + the channelizer's shift (highlights the
  // row you're on).
  let vfo = $derived.by(() => {
    const c = currentAxes(pipeline)?.center_freq_hz;
    if (c === undefined) return null;
    const shift = (pipeline.blocks.chan?.values as Record<string, number> | undefined)
      ?.freq_shift_hz;
    return c + (shift ?? 0);
  });

  const tuneTo = (r: EmitterRow) => void pipeline.tune(r.freq_hz);
</script>

<div
  class="flex h-full w-full min-h-0 flex-col border-l border-slate-800 bg-[color:var(--color-bg)]"
>
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">Signals</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {rows.length} emitter{rows.length === 1 ? '' : 's'} in span ·
        {#if signals.total > signals.signals.length}
          {signals.signals.length} of {signals.total} detections
        {:else}
          {signals.signals.length} detections
        {/if}
        {#if hidden > 0}· {hidden} ignored{/if}
      </span>
    </span>
  </header>

  <div class="min-h-0 flex-1 overflow-auto">
    <EmitterTable
      {rows}
      onPick={tuneTo}
      onListen={tuneTo}
      activeHz={vfo}
      empty="scanning the wideband FFT for signals…"
    />
  </div>

  {#if signalsBlock}
    <footer class="border-t border-slate-800 p-2">
      <BlockParams block={signalsBlock} only={['threshold_db', 'top_k']} />
    </footer>
  {/if}
</div>

<style></style>
