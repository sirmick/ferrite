<script lang="ts">
  // The spectrum analyzer's quick bar — the knobs you touch every few
  // seconds. Range / RBW / averages are the sweep block's params (server
  // state, every tab and the AI see them); gain / antenna are the radio's
  // (LiveControls, same as the receiver); trace mode / dB range are this
  // tab's display. Set-and-forget sweep knobs live in Settings.
  import LiveControls from '$lib/controls/LiveControls.svelte';
  import { parseFreq } from '$lib/controls/freqParse';
  import { pipeline } from '$lib/pipeline.svelte';
  import { specan } from './specan.svelte';
  import { fmtHz, type TraceMode } from './panorama';
  import IconZoomOut from '@tabler/icons-svelte-runes/icons/zoom-out';
  import IconEraser from '@tabler/icons-svelte-runes/icons/eraser';

  const RBW = [1000, 2500, 5000, 10000, 25000, 100000];
  const AVG = [1, 4, 8, 16, 32, 64, 128];
  const MODES: { v: TraceMode; label: string; hint: string }[] = [
    { v: 'live', label: 'Live', hint: 'Latest pass' },
    { v: 'max', label: 'Max', hint: 'Max hold since cleared' },
    { v: 'min', label: 'Min', hint: 'Min hold since cleared' },
    { v: 'avg', label: 'Avg', hint: 'Running average' },
  ];

  let src = $derived((pipeline.blocks.src?.values ?? {}) as Record<string, number>);
  let rbw = $derived(src.rbw_hz ?? 2500);
  let averages = $derived(src.averages ?? 16);
  let lo = $derived(specan.range[0]);
  let hi = $derived(specan.range[1]);
  let centreSpan = $state(false);

  let first = $state('');
  let second = $state('');
  // Re-seed the text fields whenever the server's range moves.
  $effect(() => {
    if (centreSpan) {
      first = fmtHz((lo + hi) / 2, hi - lo);
      second = fmtHz(hi - lo, hi - lo);
    } else {
      first = fmtHz(lo, hi - lo);
      second = fmtHz(hi, hi - lo);
    }
  });

  function commit() {
    const a = parseFreq(first)?.hz;
    const b = parseFreq(second)?.hz;
    if (a === undefined || b === undefined) return;
    if (centreSpan) void specan.setRange(a - b / 2, a + b / 2);
    else void specan.setRange(a, b);
  }

  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'Enter') commit();
  };
  const fmtPass = (s: number | null) =>
    s === null ? '—' : s < 1 ? `${(s * 1000).toFixed(0)} ms` : `${s.toFixed(1)} s`;
</script>

<div
  class="flex flex-wrap items-center gap-x-3 gap-y-0.5 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-0.5 text-[11px] text-[color:var(--color-muted)]"
>
  <button
    type="button"
    class="rounded border border-slate-700 px-1.5 py-0.5 hover:border-sky-400"
    title="Toggle start/stop ⇄ centre/span"
    onclick={() => (centreSpan = !centreSpan)}>{centreSpan ? 'Centre' : 'Start'}</button
  >
  <input
    class="w-28 rounded border border-slate-800 bg-slate-900 px-1.5 py-0.5 font-mono text-slate-200"
    bind:value={first}
    onkeydown={onKey}
    onblur={commit}
    aria-label={centreSpan ? 'Centre frequency' : 'Start frequency'}
  />
  <span>{centreSpan ? 'Span' : 'Stop'}</span>
  <input
    class="w-28 rounded border border-slate-800 bg-slate-900 px-1.5 py-0.5 font-mono text-slate-200"
    bind:value={second}
    onkeydown={onKey}
    onblur={commit}
    aria-label={centreSpan ? 'Span' : 'Stop frequency'}
  />
  <button
    type="button"
    class="rounded border border-slate-700 p-0.5 hover:border-sky-400"
    title="Zoom out ×2 (or right-click the trace)"
    aria-label="Zoom out"
    onclick={() => void specan.zoomOut()}><IconZoomOut size={14} /></button
  >
  <label class="flex items-center gap-1" title="Resolution (bin spacing)">
    RBW
    <select
      class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
      value={String(rbw)}
      onchange={(e) => void pipeline.patchSourceParams({ rbw_hz: Number(e.currentTarget.value) })}
    >
      {#each RBW as r (r)}
        <option value={String(r)}>{r >= 1000 ? `${r / 1000} kHz` : `${r} Hz`}</option>
      {/each}
      {#if !RBW.includes(rbw)}<option value={String(rbw)}>{rbw} Hz</option>{/if}
    </select>
  </label>
  <label class="flex items-center gap-1" title="FFT frames averaged per step">
    Avg
    <select
      class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
      value={String(averages)}
      onchange={(e) => void pipeline.patchSourceParams({ averages: Number(e.currentTarget.value) })}
    >
      {#each AVG as a (a)}<option value={String(a)}>{a}</option>{/each}
    </select>
  </label>
  <span class="font-mono" title="Measured time per full pass"
    >pass {fmtPass(specan.passSeconds)}</span
  >
  <LiveControls />
  <div class="ml-auto flex items-center gap-2">
    <div
      class="flex overflow-hidden rounded border border-slate-700"
      role="group"
      aria-label="Trace"
    >
      {#each MODES as m (m.v)}
        <button
          type="button"
          class="px-1.5 py-0.5 hover:bg-slate-800"
          class:mode-on={specan.traceMode === m.v}
          title={m.hint}
          onclick={() => (specan.traceMode = m.v)}>{m.label}</button
        >
      {/each}
    </div>
    <button
      type="button"
      class="flex items-center gap-1 rounded border border-slate-700 px-1.5 py-0.5 hover:border-sky-400 disabled:opacity-40"
      title="Reset the max / min / average holds (they also reset when the range changes)"
      disabled={specan.traceMode === 'live'}
      onclick={() => specan.clearToken++}><IconEraser size={14} /> Reset hold</button
    >
    <label class="flex items-center gap-1" title="Auto-scale the dB axis">
      <input type="checkbox" bind:checked={specan.autoDb} /> auto dB
    </label>
    {#if !specan.autoDb}
      <input
        type="number"
        class="w-14 rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
        bind:value={specan.dbLo}
        aria-label="dB bottom"
      />
      <input
        type="number"
        class="w-14 rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
        bind:value={specan.dbHi}
        aria-label="dB top"
      />
    {/if}
  </div>
</div>

<style>
  .mode-on {
    background: rgba(125, 211, 252, 0.15);
    color: #e2e8f0;
  }
</style>
