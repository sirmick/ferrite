<script lang="ts">
  // Curated "quick" source controls — gain, antenna, AGC. Mounted next
  // to the nixies in the spectrum header. Every commit goes through
  // `pipeline.patchSourceParams`; the server decides whether the delta
  // is live-applicable (whitelist: `gain_db`, `antenna`, `agc`,
  // `center_freq_hz`) or needs a rebuild. Same wire path as the advanced
  // panel, just a curated subset of controls.

  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';

  let caps = $derived(
    pipeline.sourceCaps?.kind === 'hardware' ? pipeline.sourceCaps.capabilities : null,
  );
  let channel = $derived(caps?.rx_channels[0] ?? null);
  let params = $derived((pipeline.source?.params ?? {}) as Record<string, unknown>);

  let overallRange = $derived(channel?.overall_gain_range_db ?? null);
  let antennas = $derived(channel?.antennas ?? []);
  let hasAgc = $derived(channel?.has_agc ?? false);

  let gainDb = $derived(numberOr(params.gain_db, overallRange?.min ?? 0));
  let antenna = $derived(typeof params.antenna === 'string' ? (params.antenna as string) : '');
  let agc = $derived(params.agc === true);

  let visible = $derived(!!channel && (overallRange !== null || antennas.length > 1 || hasAgc));

  function numberOr(v: unknown, fallback: number): number {
    const n = typeof v === 'number' ? v : Number(v);
    return Number.isFinite(n) ? n : fallback;
  }

  // Every commit goes through `patchSourceParams`. The server inspects
  // the delta: whitelisted keys live-apply (no rebuild); others trigger
  // full source reconstruction. Kept deliberately simple here — no
  // client-side branching on which keys are hot. Same wire path the
  // advanced panel uses.

  // Trailing-edge debounce for the scrubbable gain slider. `oninput`
  // fires on every pixel of the drag; without this we'd ship ~60
  // patches/sec for a one-second drag, overwhelming the hot path even
  // though Soapy itself would accept them all. 50ms smooths the drag to
  // ~20 patches/sec — imperceptible lag, vastly less churn.
  const GAIN_DEBOUNCE_MS = 50;
  let gainDebounce: ReturnType<typeof setTimeout> | undefined;
  let pendingGain: number | undefined;

  function commitGain(v: number) {
    if (v === gainDb) return;
    pendingGain = v;
    if (gainDebounce !== undefined) clearTimeout(gainDebounce);
    gainDebounce = setTimeout(() => {
      gainDebounce = undefined;
      const next = pendingGain;
      pendingGain = undefined;
      if (next !== undefined && next !== gainDb) {
        void applyControl('flow.src.gain_db', next);
      }
    }, GAIN_DEBOUNCE_MS);
  }
  function commitAntenna(v: string) {
    if (v === antenna) return;
    void applyControl('flow.src.antenna', v);
  }
  function commitAgc(v: boolean) {
    if (v === agc) return;
    void applyControl('flow.src.agc', v);
  }
</script>

{#if visible}
  <div class="mx-1 h-4 border-l border-slate-800"></div>

  {#if overallRange}
    <label class="flex items-center gap-1" title="receiver gain (dB)">
      <span>gain</span>
      <input
        type="range"
        class="w-24"
        min={overallRange.min}
        max={overallRange.max}
        step={overallRange.step ?? 1}
        value={gainDb}
        oninput={(e) => commitGain(Number((e.currentTarget as HTMLInputElement).value))}
      />
      <span class="w-10 text-right font-mono text-slate-300">{gainDb.toFixed(1)}</span>
      <span>dB</span>
    </label>
  {/if}

  {#if antennas.length > 1}
    <label class="flex items-center gap-1" title="RF antenna port">
      <span>ant</span>
      <select
        class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
        value={antenna}
        onchange={(e) => commitAntenna((e.currentTarget as HTMLSelectElement).value)}
      >
        {#each antennas as a (a)}
          <option value={a}>{a}</option>
        {/each}
      </select>
    </label>
  {/if}

  {#if hasAgc}
    <label class="flex items-center gap-1" title="automatic gain control">
      <input
        type="checkbox"
        checked={agc}
        onchange={(e) => commitAgc((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>agc</span>
    </label>
  {/if}
{/if}
