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
  let viewZoom = $derived(clientControls.get('client.spectrum.viewZoom'));
  // Waterfall contrast (separate from the spectrum trace's display
  // range — the waterfall is byte-quantised and uses its own
  // percentile-based auto-track by default).
  let wfAuto = $derived(clientControls.get('client.waterfall.autoContrast'));
  let wfFloor = $derived(clientControls.get('client.waterfall.contrastFloorDbfs'));
  let wfCeil = $derived(clientControls.get('client.waterfall.contrastCeilDbfs'));
</script>

<div
  class="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
>
  {#if fftBlock || logmagBlock}
    {#if fftBlock}
      <label class="flex items-center gap-1" title="FFT bin count — higher = finer bins, slower">
        <span>fft</span>
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
          class="w-20"
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
  <label class="flex items-center gap-1" title="running max-hold trace">
    <input
      type="checkbox"
      checked={maxHold}
      onchange={(e) =>
        void applyControl('client.spectrum.maxHold', (e.currentTarget as HTMLInputElement).checked)}
    />
    <span>max hold</span>
  </label>
  {#if maxHold}
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
      onclick={() => waterfallStore.resetMaxHold()}
    >
      reset
    </button>
  {/if}

  <div class="mx-1 h-4 border-l border-slate-800"></div>

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
  <span class="flex items-center gap-2 font-mono text-[10px]">
    <span class="w-8 text-right text-slate-400">{manualCeilDbfs}</span>
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
    <span class="w-10 text-slate-400">{manualFloorDbfs}</span>
  </span>

  <div class="mx-1 h-4 border-l border-slate-800"></div>

  <!-- Waterfall contrast: separate from the spectrum trace's floor/ceil
       above. The waterfall renderer can auto-stretch its palette
       window to P5/P98 of recent FFT rows (so the noise floor sits in
       dark blue and strong carriers reach bright white regardless of
       band-by-band noise differences), or use a fixed dBFS window. -->
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
    <span>wf auto</span>
  </label>
  <span
    class="flex items-center gap-2 font-mono text-[10px]"
    title="manual waterfall contrast window (dBFS)"
  >
    <span class="w-8 text-right text-slate-400">{wfCeil}</span>
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
    <span class="w-10 text-slate-400">{wfFloor}</span>
  </span>

  <div class="mx-1 h-4 border-l border-slate-800"></div>

  <label
    class="flex items-center gap-1"
    title="display zoom — crops the FFT view; SDR sample rate is unchanged"
  >
    <span>zoom</span>
    <input
      type="range"
      class="w-20"
      min={1}
      max={16}
      step={0.5}
      value={viewZoom}
      oninput={(e) =>
        void applyControl(
          'client.spectrum.viewZoom',
          Number((e.currentTarget as HTMLInputElement).value),
        )}
    />
    <span class="w-7 text-right font-mono text-slate-300">{viewZoom.toFixed(1)}×</span>
    {#if viewZoom > 1}
      <button
        type="button"
        class="rounded border border-slate-700 px-1 py-0 text-[10px] leading-none hover:border-slate-500"
        title="reset zoom + pan"
        onclick={() => {
          void applyControl('client.spectrum.viewZoom', 1);
          void applyControl('client.spectrum.viewPan', 0.5);
        }}
      >
        ⟲
      </button>
    {/if}
  </label>

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
