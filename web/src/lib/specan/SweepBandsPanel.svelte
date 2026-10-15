<script lang="ts">
  // Bands, spectrum-analyzer flavour: clicking sweeps the range rather
  // than tuning. A short list of useful VHF/UHF spans for the station.
  import { specan } from './specan.svelte';
  import { fmtHz } from './panorama';

  const RANGES: { name: string; lo: number; hi: number; note?: string }[] = [
    { name: 'FM broadcast', lo: 87.5e6, hi: 108e6 },
    { name: 'Airband (AM)', lo: 108e6, hi: 137e6 },
    { name: '2 m amateur', lo: 144e6, hi: 148e6 },
    { name: 'Marine VHF + NOAA WX', lo: 155e6, hi: 163e6 },
    { name: 'VHF high (business / public safety)', lo: 148e6, hi: 174e6 },
    { name: '70 cm amateur', lo: 420e6, hi: 450e6 },
    { name: 'UHF business', lo: 450e6, hi: 470e6 },
    { name: 'ISM 433', lo: 432e6, hi: 436e6 },
    { name: 'FRS / GMRS', lo: 462e6, hi: 468e6 },
    { name: 'ISM 902–928', lo: 902e6, hi: 928e6 },
    { name: 'ADS-B / Mode S', lo: 1080e6, hi: 1100e6 },
    { name: 'VHF 30–300 MHz', lo: 30e6, hi: 300e6, note: '~36 steps' },
    { name: 'UHF 300–1000 MHz', lo: 300e6, hi: 1000e6, note: '~92 steps' },
    { name: 'Everything 1–2000 MHz', lo: 1e6, hi: 2000e6, note: '~260 steps' },
  ];

  const active = (r: { lo: number; hi: number }) =>
    Math.abs(r.lo - specan.range[0]) < 1 && Math.abs(r.hi - specan.range[1]) < 1;
</script>

<div class="flex h-full flex-col overflow-y-auto text-xs">
  <div class="px-2 py-1 text-[10px] uppercase tracking-wider text-[color:var(--color-muted)]">
    Sweep a range
  </div>
  {#each RANGES as r (r.name)}
    <button
      type="button"
      class="flex items-baseline justify-between gap-2 border-b border-slate-900 px-2 py-1 text-left hover:bg-slate-900"
      class:row-on={active(r)}
      onclick={() => void specan.setRange(r.lo, r.hi)}
    >
      <span class="text-slate-200">{r.name}</span>
      <span class="font-mono text-[10px] text-slate-500"
        >{fmtHz(r.lo, r.hi - r.lo)}–{fmtHz(r.hi, r.hi - r.lo)}{r.note ? ` · ${r.note}` : ''}</span
      >
    </button>
  {/each}
  <p class="px-2 py-2 text-[10px] text-slate-500">
    Or drag across the trace to sweep a selection; right-click zooms out; double-click listens
    there.
  </p>
</div>

<style>
  .row-on {
    background: rgba(125, 211, 252, 0.08);
  }
</style>
