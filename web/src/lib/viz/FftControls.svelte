<script lang="ts">
  // Compact FFT controls strip — renders the FFT and LogMagU8 block
  // knobs that belong next to the spectrum, not in the receiver pane.
  // Shown only when the composed preset exposes the canonical block
  // ids (`fft`, `logmag`). The strip reads live values from
  // `pipeline.blocks[id].values` and commits via `setBlockParam`, so
  // the knobs always reflect what the runtime is actually using.

  import { pipeline } from '$lib/pipeline.svelte';

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
  let floorDb = $derived(numValue(logmagBlock, 'floor_dbfs', -100));
  let ceilDb = $derived(numValue(logmagBlock, 'ceil_dbfs', 0));
  let alpha = $derived(numValue(logmagBlock, 'alpha', 0.3));

  // Keeping `fft.size` and `logmag.size` in sync is a UI concern —
  // the blocks don't enforce it themselves. The size knob is a
  // single control here and fans out to both blocks.
  async function commitSize(v: number) {
    if (fftBlock) await pipeline.setBlockParam(FFT_ID, 'size', v);
    if (logmagBlock) await pipeline.setBlockParam(LOGMAG_ID, 'size', v);
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
            void pipeline.setBlockParam(FFT_ID, 'window', (e.target as HTMLSelectElement).value)}
        >
          {#each windowChoices as w (w)}
            <option value={w}>{w}</option>
          {/each}
        </select>
      </label>
    {/if}

    {#if logmagBlock}
      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1" title="bottom of the visible dBFS range">
        <span>floor</span>
        <input
          type="number"
          class="w-16 rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          min={-160}
          max={0}
          step={1}
          value={floorDb}
          onchange={(e) =>
            void pipeline.setBlockParam(
              LOGMAG_ID,
              'floor_dbfs',
              Number((e.target as HTMLInputElement).value),
            )}
        />
        <span>dBFS</span>
      </label>
      <label class="flex items-center gap-1" title="top of the visible dBFS range">
        <span>ceil</span>
        <input
          type="number"
          class="w-16 rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          min={-60}
          max={60}
          step={1}
          value={ceilDb}
          onchange={(e) =>
            void pipeline.setBlockParam(
              LOGMAG_ID,
              'ceil_dbfs',
              Number((e.target as HTMLInputElement).value),
            )}
        />
        <span>dBFS</span>
      </label>
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
            void pipeline.setBlockParam(
              LOGMAG_ID,
              'alpha',
              Number((e.target as HTMLInputElement).value),
            )}
        />
        <span class="w-8 font-mono text-slate-300">{alpha.toFixed(2)}</span>
      </label>
    {/if}
  </div>
{/if}
