<script lang="ts">
  // RDS (Radio Data System) readout — station name, PI, PTY. Appears
  // in the Settings panel when a preset wires `ui:rds`; silently
  // hides itself otherwise so the SSB / AM / digital presets show no
  // empty box.

  import { pipeline } from '$lib/pipeline.svelte';
  import { rds, PTY_NAMES } from './store.svelte';

  let streamId = $derived(pipeline.uiSinks.rds?.stream_id);
  let client = $derived(pipeline.client);

  $effect(() => {
    if (client && streamId !== undefined) {
      rds.attach(client, streamId);
      return () => rds.detach();
    }
    rds.detach();
    return () => {};
  });

  let ptyLabel = $derived(rds.pty === null ? null : (PTY_NAMES[rds.pty] ?? `PTY ${rds.pty}`));
  // PI is a 4-hex-digit code — render in upper-case for the display
  // convention every broadcaster database uses (e.g. RDS Spy, RDSVisual).
  let piHex = $derived(rds.pi === null ? null : rds.pi.toString(16).toUpperCase().padStart(4, '0'));
</script>

{#if streamId !== undefined}
  <div class="flex flex-col gap-1 text-xs">
    <div class="flex items-baseline justify-between">
      <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
        station
      </span>
      <span class="font-mono text-slate-300">
        {#if rds.ps}
          {rds.ps}
        {:else}
          <span class="text-slate-600">—</span>
        {/if}
      </span>
    </div>
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
