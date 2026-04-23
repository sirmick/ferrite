<script lang="ts">
  // Compact FFT controls strip — renders the FFT and LogMagU8 block
  // knobs that belong next to the spectrum, not in the receiver pane.
  // Shown only when the composed preset exposes the canonical block
  // ids (`fft`, `logmag`). The strip reads live values from
  // `pipeline.blocks[id].values` and commits via `applyControl`.
  //
  // Floor/ceil used to live here as manual logmag params; they moved
  // client-side (`client.spectrum.displayFloorDbfs`/`displayCeilDbfs`)
  // once the server quantisation window became fixed — no UI inputs
  // here today, the auto-scale toggle in the spectrum strip is the
  // primary way users adjust the visible range.

  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';

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

  // The enum_numeric values for `size` come from the block spec — by
  // convention the FFT and LogMagU8 declare the same ladder, so we
  // pull from whichever is present.
  let sizeChoices = $derived.by<number[]>(() => {
    const spec = fftBlock?.spec.params.find((p) => p.key === 'size');
    if (spec && spec.kind === 'enum_numeric') return [...spec.values];
    return [1024, 2048, 4096, 8192, 16384];
  });
  let windowChoices = $derived.by<string[]>(() => {
    const spec = fftBlock?.spec.params.find((p) => p.key === 'window');
    if (spec && spec.kind === 'enum_string') return [...spec.values];
    return ['none', 'hann', 'hamming', 'blackman'];
  });

  let size = $derived(numValue(fftBlock, 'size', 4096));
  let windowKind = $derived(strValue(fftBlock, 'window', 'hann'));
  let alpha = $derived(numValue(logmagBlock, 'alpha', 0.3));

  // Both `fft.size` and `logmag.size` must change together — logmag
  // expects the fft's output bin count, so mid-flight they can't
  // disagree even for one tick. Two sequential writes rebuild the
  // blocks out of phase and the display ends up empty or malformed;
  // `patchFlowgraph` swaps the whole doc atomically so both blocks
  // see the new size in the same reconfigure pass. This is the one
  // structural write from the FFT strip, hence the direct call — it
  // doesn't fit the single-key `applyControl` shape.
  async function commitSize(v: number) {
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
</script>

{#if fftBlock || logmagBlock}
  <div
    class="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-slate-800/60 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
  >
    {#if fftBlock}
      <label class="flex items-center gap-1" title="FFT bin count — higher = finer bins, slower">
        <span>size</span>
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          value={size}
          onchange={(e) => void commitSize(Number((e.target as HTMLSelectElement).value))}
        >
          {#each sizeChoices as s (s)}
            <option value={s}>{s}</option>
          {/each}
        </select>
      </label>
      <label class="flex items-center gap-1" title="FFT window function">
        <span>window</span>
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          value={windowKind}
          onchange={(e) =>
            void applyControl(`flow.${FFT_ID}.window`, (e.target as HTMLSelectElement).value)}
        >
          {#each windowChoices as w (w)}
            <option value={w}>{w}</option>
          {/each}
        </select>
      </label>
    {/if}

    {#if logmagBlock}
      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1" title="EMA smoothing across rows (1 = no smoothing)">
        <span>smooth</span>
        <input
          type="range"
          class="w-20"
          min={0.01}
          max={1}
          step={0.01}
          value={alpha}
          onchange={(e) =>
            void applyControl(
              `flow.${LOGMAG_ID}.alpha`,
              Number((e.target as HTMLInputElement).value),
            )}
        />
        <span class="w-8 font-mono text-slate-300">{alpha.toFixed(2)}</span>
      </label>
    {/if}
  </div>
{/if}
