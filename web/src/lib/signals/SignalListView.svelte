<script lang="ts">
  // Strongest-signal list — occupies the right (channel-detail) column
  // in place of the narrow FFT/waterfall when the operator picks
  // Display → Right → "Strongest Signals" (only offered when the active
  // preset advertises a `ui:signals` sink, i.e. wires the SignalList
  // block at the wideband FFT). Rows are the auto-detected signals,
  // ranked strongest-first; clicking a row tunes the VFO to that
  // frequency. Keyed on the block's stable track `id` so rows update in
  // place rather than re-rendering as signals come and go.

  import { pipeline } from '$lib/pipeline.svelte';
  import { signals } from '$lib/signals/store.svelte';
  import BlockParams from '$lib/controls/BlockParams.svelte';

  // The watchlist comes from the always-connected decoder mirror via the
  // `signals` adapter — no per-view WS attach. The node-side SignalList
  // detector backing this panel has live Min SNR (detection threshold) +
  // Top N knobs, so editing them re-ranks the list + FFT squares at once.
  let signalsBlock = $derived(pipeline.blocks['signals']);

  // MHz with enough digits to distinguish adjacent broadcast channels.
  function fmtMhz(hz: number): string {
    return (hz / 1e6).toFixed(4);
  }
  // Signal occupied bandwidth, in the friendliest unit.
  function fmtBw(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(2)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kHz`;
    return `${Math.round(hz)} Hz`;
  }

  function tuneTo(freqHz: number): void {
    void pipeline.tune(freqHz);
  }
</script>

<div
  class="flex h-full w-full min-h-0 flex-col border-l border-slate-800 bg-[color:var(--color-bg)]"
>
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">Signals</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {signals.signals.length} strongest
      </span>
    </span>
  </header>

  <div class="min-h-0 flex-1 overflow-auto">
    {#if signals.signals.length === 0}
      <p class="p-3 text-[11px] text-[color:var(--color-muted)]">
        scanning the wideband FFT for signals…
      </p>
    {:else}
      <table class="w-full border-collapse text-[11px] tabular-nums">
        <thead
          class="sticky top-0 bg-[color:var(--color-bg)] text-left text-[color:var(--color-muted)]"
        >
          <tr class="border-b border-slate-800">
            <th class="px-2 py-1 font-medium">Freq (MHz)</th>
            <th class="px-2 py-1 text-right font-medium">Pwr</th>
            <th class="px-2 py-1 text-right font-medium">SNR</th>
            <th class="px-2 py-1 text-right font-medium">BW</th>
          </tr>
        </thead>
        <tbody>
          {#each signals.signals as sig (sig.id)}
            <tr
              class="cursor-pointer border-b border-slate-900 hover:bg-sky-900/30"
              title="Tune to {fmtMhz(sig.freq_hz)} MHz"
              onclick={() => tuneTo(sig.freq_hz)}
            >
              <td class="px-2 py-1 font-mono text-sky-300">{fmtMhz(sig.freq_hz)}</td>
              <td class="px-2 py-1 text-right">{sig.power_db.toFixed(1)}</td>
              <td class="px-2 py-1 text-right text-emerald-300">{sig.snr_db.toFixed(0)}</td>
              <td class="px-2 py-1 text-right text-[color:var(--color-muted)]">
                {fmtBw(sig.bw_hz)}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>

  {#if signalsBlock}
    <footer class="border-t border-slate-800 p-2">
      <BlockParams block={signalsBlock} only={['threshold_db', 'top_k']} />
    </footer>
  {/if}
</div>

<style></style>
