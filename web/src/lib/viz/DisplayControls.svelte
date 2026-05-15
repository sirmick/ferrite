<script lang="ts">
  // Display knobs that used to live in the Spectrum top bar — moved to
  // a dedicated strip beneath the waterfall so the spectrum header can
  // stay focused on RF (VFO/SDR/rate/gain). Splitting RF from "what the
  // FFT looks like" controls makes both strips shorter and each pane's
  // mental model cleaner.
  //
  // Reads/writes go through the same `applyControl` + `clientControls`
  // path as before; the maximum-hold reset reaches into the Spectrum
  // renderer via the `waterfallStore.resetMaxHold` slot that Spectrum
  // wires up on mount.

  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { waterfallStore } from './waterfallStore.svelte';
  import DbRangeSlider from './DbRangeSlider.svelte';

  // FFT/LogMag block accessors (same shape as Spectrum.svelte's old
  // FftControls inline copy — kept local so DisplayControls stands on
  // its own without a shared base module).
  const FFT_ID = 'fft';
  const LOGMAG_ID = 'logmag';
  let fftBlock = $derived(pipeline.blocks[FFT_ID]);
  let logmagBlock = $derived(pipeline.blocks[LOGMAG_ID]);
  function numValue(block: typeof fftBlock | undefined, key: string, fallback: number): number {
    const v = (block?.values as Record<string, unknown> | null | undefined)?.[key];
    return typeof v === 'number' ? v : fallback;
  }
  function strValue(block: typeof fftBlock | undefined, key: string, fallback: string): string {
    const v = (block?.values as Record<string, unknown> | null | undefined)?.[key];
    return typeof v === 'string' ? v : fallback;
  }
  let fftSizeChoices = $derived.by<number[]>(() => {
    const spec = fftBlock?.spec.params.find((p) => p.key === 'size');
    if (spec && spec.kind === 'enum_numeric') return [...spec.values];
    return [1024, 2048, 4096, 8192, 16384];
  });
  let fftWindowChoices = $derived.by<string[]>(() => {
    const spec = fftBlock?.spec.params.find((p) => p.key === 'window');
    if (spec && spec.kind === 'enum_string') return [...spec.values];
    return ['none', 'hann', 'hamming', 'blackman'];
  });
  let fftSize = $derived(numValue(fftBlock, 'size', 4096));
  let fftWindowKind = $derived(strValue(fftBlock, 'window', 'hann'));
  let logmagAlpha = $derived(numValue(logmagBlock, 'alpha', 0.3));

  async function commitFftSize(v: number) {
    const doc = pipeline.flowgraph;
    if (!doc) return;
    const nextBlocks: Record<string, unknown> = {};
    for (const [id, block] of Object.entries(doc.blocks ?? {})) {
      if (id === FFT_ID || id === LOGMAG_ID) {
        const p = (block.params ?? {}) as Record<string, unknown>;
        nextBlocks[id] = { ...block, params: { ...p, size: v } };
      } else {
        nextBlocks[id] = block;
      }
    }
    await pipeline.patchFlowgraph({ ...doc, blocks: nextBlocks as typeof doc.blocks });
  }

  // Display flags + ranges
  let bandPlan = $derived(clientControls.get('client.spectrum.bandPlan'));
  let fade = $derived(clientControls.get('client.spectrum.fade'));
  let maxHold = $derived(clientControls.get('client.spectrum.maxHold'));
  let autoScale = $derived(clientControls.get('client.spectrum.autoScale'));
  let manualFloorDbfs = $derived(clientControls.get('client.spectrum.displayFloorDbfs'));
  let manualCeilDbfs = $derived(clientControls.get('client.spectrum.displayCeilDbfs'));
  // Waterfall contrast (separate from the spectrum trace's display
  // range — the waterfall is byte-quantised and uses its own
  // percentile-based auto-track by default).
  let wfAuto = $derived(clientControls.get('client.waterfall.autoContrast'));
  let wfFloor = $derived(clientControls.get('client.waterfall.contrastFloorDbfs'));
  let wfCeil = $derived(clientControls.get('client.waterfall.contrastCeilDbfs'));
</script>

<!-- Layout reads l → r as DSP shaping → display flags → spectrum
     range → waterfall range → zoom → pause. The exact-value dBFS
     readouts that used to flank each range slider are gone — the
     dual-thumb DbRangeSlider visualises the window directly, and a
     hover title gives the numbers for callers who really need them.
     Saved ~140 px per slider and the strip now fits on a 1366-wide
     viewport without flex-wrapping. -->
<div
  class="flex w-full flex-wrap items-center gap-x-2 gap-y-1 border-t border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
>
  {#if fftBlock || logmagBlock}
    {#if fftBlock}
      <label class="flex items-center gap-1" title="FFT bin count — higher = finer bins, slower">
        <span>bins</span>
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          value={fftSize}
          onchange={(e) => void commitFftSize(Number((e.target as HTMLSelectElement).value))}
        >
          {#each fftSizeChoices as s (s)}
            <option value={s}>{s}</option>
          {/each}
        </select>
      </label>
      <label class="flex items-center gap-1" title="FFT window function">
        <span>win</span>
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          value={fftWindowKind}
          onchange={(e) =>
            void applyControl(`flow.${FFT_ID}.window`, (e.target as HTMLSelectElement).value)}
        >
          {#each fftWindowChoices as w (w)}
            <option value={w}>{w}</option>
          {/each}
        </select>
      </label>
    {/if}
    {#if logmagBlock}
      <label class="flex items-center gap-1" title="EMA smoothing across rows (1 = no smoothing)">
        <span>smooth</span>
        <input
          type="range"
          class="w-16"
          min={0.01}
          max={1}
          step={0.01}
          value={logmagAlpha}
          oninput={(e) =>
            void applyControl(
              `flow.${LOGMAG_ID}.alpha`,
              Number((e.target as HTMLInputElement).value),
            )}
        />
      </label>
    {/if}
    <div class="mx-1 h-4 border-l border-slate-800"></div>
  {/if}

  <label class="flex items-center gap-1" title="frequency allocation ribbon (US)">
    <input
      type="checkbox"
      checked={bandPlan}
      onchange={(e) =>
        void applyControl(
          'client.spectrum.bandPlan',
          (e.currentTarget as HTMLInputElement).checked,
        )}
    />
    <span>bands</span>
  </label>
  <label class="flex items-center gap-1" title="fade trail of recent traces">
    <input
      type="checkbox"
      checked={fade}
      onchange={(e) =>
        void applyControl('client.spectrum.fade', (e.currentTarget as HTMLInputElement).checked)}
    />
    <span>fade</span>
  </label>
  <label class="flex items-center gap-1" title="running peak-hold trace">
    <input
      type="checkbox"
      checked={maxHold}
      onchange={(e) =>
        void applyControl('client.spectrum.maxHold', (e.currentTarget as HTMLInputElement).checked)}
    />
    <span>peak</span>
  </label>
  {#if maxHold}
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
      title="reset peak-hold trace"
      onclick={() => waterfallStore.resetMaxHold()}
    >
      ⟲
    </button>
  {/if}

  <div class="mx-1 h-4 border-l border-slate-800"></div>

  <!-- Spectrum-trace dB range (the line plot). Auto-track follows
       signal envelope; manual is the dual-thumb slider. -->
  <span class="text-slate-400">spec</span>
  <label
    class="flex items-center gap-1"
    title="auto-track floor/ceil to the signal (overrides the manual values)"
  >
    <input
      type="checkbox"
      checked={autoScale}
      onchange={(e) =>
        void applyControl(
          'client.spectrum.autoScale',
          (e.currentTarget as HTMLInputElement).checked,
        )}
    />
    <span>auto</span>
  </label>
  <span
    class="flex items-center font-mono text-[10px]"
    title="spectrum trace range: {manualFloorDbfs} … {manualCeilDbfs} dBFS"
  >
    <DbRangeSlider
      floor={manualFloorDbfs}
      ceil={manualCeilDbfs}
      disabled={autoScale}
      onChange={(next) => {
        if (next.floor !== manualFloorDbfs) {
          void applyControl('client.spectrum.displayFloorDbfs', next.floor);
        }
        if (next.ceil !== manualCeilDbfs) {
          void applyControl('client.spectrum.displayCeilDbfs', next.ceil);
        }
      }}
    />
  </span>

  <div class="mx-1 h-4 border-l border-slate-800"></div>

  <!-- Waterfall colormap window. Default auto-stretches to P5/P98
       of recent FFT rows so the noise floor sits in dark blue and
       strong carriers reach bright white regardless of the band's
       absolute level. Manual mode uses the fixed dBFS window. -->
  <span class="text-slate-400">wf</span>
  <label
    class="flex items-center gap-1"
    title="auto-stretch the waterfall colormap to recent P5/P98 of the byte stream"
  >
    <input
      type="checkbox"
      checked={wfAuto}
      onchange={(e) =>
        void applyControl(
          'client.waterfall.autoContrast',
          (e.currentTarget as HTMLInputElement).checked,
        )}
    />
    <span>auto</span>
  </label>
  <span
    class="flex items-center font-mono text-[10px]"
    title="waterfall contrast window: {wfFloor} … {wfCeil} dBFS"
  >
    <DbRangeSlider
      floor={wfFloor}
      ceil={wfCeil}
      disabled={wfAuto}
      onChange={(next) => {
        if (next.floor !== wfFloor) {
          void applyControl('client.waterfall.contrastFloorDbfs', next.floor);
        }
        if (next.ceil !== wfCeil) {
          void applyControl('client.waterfall.contrastCeilDbfs', next.ceil);
        }
      }}
    />
  </span>

  <div class="mx-1 h-4 border-l border-slate-800"></div>

  <button
    type="button"
    class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
    class:bg-amber-900={waterfallStore.paused}
    onclick={() => (waterfallStore.paused = !waterfallStore.paused)}
    title={waterfallStore.paused ? 'resume waterfall' : 'freeze waterfall'}
  >
    {waterfallStore.paused ? '▶' : '❚❚'}
  </button>
</div>
