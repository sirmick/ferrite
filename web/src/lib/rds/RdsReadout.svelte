<script lang="ts">
  // RDS (Radio Data System) readout — station name, RadioText, PI, PTY. Appears
  // in the Settings panel when a preset wires `ui:rds`; silently
  // hides itself otherwise so the SSB / AM / digital presets show no
  // empty box.

  import { pipeline } from '$lib/pipeline.svelte';
  import { rds, PTY_NAMES } from './store.svelte';
  import { rbdsCallsign } from './callsign';

  // RDS state comes from the always-connected decoder mirror (kind `rds`,
  // Merge-folded). Show the readout when the preset has an RdsDemod.
  let hasRds = $derived(pipeline.blocks.rds !== undefined);

  let ptyLabel = $derived(rds.pty === null ? null : (PTY_NAMES[rds.pty] ?? `PTY ${rds.pty}`));
  // PI is a 4-hex-digit code — render in upper-case for the display
  // convention every broadcaster database uses (e.g. RDS Spy, RDSVisual).
  let piHex = $derived(rds.pi === null ? null : rds.pi.toString(16).toUpperCase().padStart(4, '0'));
  // The call sign stays put while many US stations scroll a slogan or
  // the song title through the 8-char PS.
  let callsign = $derived(rbdsCallsign(rds.pi));
</script>

{#if hasRds}
  <div class="flex flex-col gap-1 text-xs">
    <div class="flex items-baseline justify-between">
      <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
        station
      </span>
      <span class="font-mono text-slate-300">
        {#if callsign}<span class="text-sky-300">{callsign}</span>{/if}
        {#if rds.ps}
          <span class="whitespace-pre">{rds.ps}</span>
        {:else if !callsign}
          <span class="text-slate-600">—</span>
        {/if}
      </span>
    </div>
    {#if rds.rt}
      <div class="font-mono text-[11px] leading-snug text-slate-400" title={rds.rt}>{rds.rt}</div>
    {/if}
    <div class="flex items-baseline justify-between text-[10px]">
      <span class="text-[color:var(--color-muted)]">pi</span>
      <span class="font-mono text-slate-400">
        {#if piHex}{piHex}{:else}—{/if}
      </span>
    </div>
    <div class="flex items-baseline justify-between text-[10px]">
      <span class="text-[color:var(--color-muted)]">type</span>
      <span class="font-mono text-slate-400">
        {#if ptyLabel}{ptyLabel}{:else}—{/if}
      </span>
    </div>
    {#if rds.tp || rds.ta}
      <div class="flex gap-2 text-[10px] text-[color:var(--color-muted)]">
        {#if rds.tp}
          <span class="rounded-sm bg-amber-900/40 px-1 font-mono text-amber-300">TP</span>
        {/if}
        {#if rds.ta}
          <span class="rounded-sm bg-rose-900/40 px-1 font-mono text-rose-300">TA</span>
        {/if}
      </div>
    {/if}
  </div>
{/if}
