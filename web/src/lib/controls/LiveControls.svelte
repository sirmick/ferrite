<script lang="ts">
  // Hot/live source controls — gain, antenna, AGC. Mounted in the spectrum
  // header next to the nixies (#70). Each commit goes through
  // `pipeline.patchSourceParams`, which the server applies live for the
  // whitelisted keys (`gain_db`, `antenna`, `agc`, `center_freq_hz`)
  // without rebuilding the flowgraph. Driver-specific knobs live in the
  // Settings → Input panel and restart on change.

  import { pipeline } from '$lib/pipeline.svelte';

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

  // Routes through `setBlockParam('src', …)`, which on the server hits
  // `apply_live_params` for the whitelisted hot keys (`gain_db`,
  // `antenna`, `agc`, `center_freq_hz`). That's the no-rebuild path.
  // Going through `patchSourceParams` here would trigger a full source
  // teardown + reopen on every drag tick, which thrashes the driver
  // (RTL-SDR's R820T loses PLL lock on rapid reopens).
  const SRC_ID = 'src';

  function commitGain(v: number) {
    if (v === gainDb) return;
    void pipeline.setBlockParam(SRC_ID, 'gain_db', v);
  }
  function commitAntenna(v: string) {
    if (v === antenna) return;
    void pipeline.setBlockParam(SRC_ID, 'antenna', v);
  }
  function commitAgc(v: boolean) {
    if (v === agc) return;
    void pipeline.setBlockParam(SRC_ID, 'agc', v);
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
