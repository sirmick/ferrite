<script lang="ts">
  // RSSI meter — horizontal bar showing smoothed IQ signal strength.
  // Subscribes to `pipeline.uiSinks.rssi` when present, and silently
  // hides itself on presets that don't include an RssiProbe wired to
  // `ui:rssi`.

  import { pipeline } from '$lib/pipeline.svelte';
  import { rssi } from './store.svelte';

  let streamId = $derived(pipeline.uiSinks.rssi?.stream_id);
  let client = $derived(pipeline.client);

  // Keep the subscription in sync with whichever stream_id the server
  // allocated for this preset. `attach` is idempotent on repeat calls
  // and `detach` runs when the effect tears down on preset reload.
  $effect(() => {
    if (client && streamId !== undefined) {
      rssi.attach(client, streamId);
      return () => rssi.detach();
    }
    rssi.detach();
    return () => {};
  });

  const MIN_DB = -90;
  const MAX_DB = 0;

  let value = $derived(rssi.dbfs);
  let fraction = $derived.by(() => {
    if (value === null || !Number.isFinite(value)) return 0;
    return Math.max(0, Math.min(1, (value - MIN_DB) / (MAX_DB - MIN_DB)));
  });
  // Strong signal: green. Moderate: yellow. Near-noise: grey.
  let colour = $derived.by(() => {
    if (value === null) return '#475569';
    if (value > -30) return '#34d399';
    if (value > -60) return '#facc15';
    return '#94a3b8';
  });
</script>

{#if streamId !== undefined}
  <div class="flex flex-col gap-1 text-xs">
    <div class="flex items-baseline justify-between">
      <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">rssi</span>
      <span class="font-mono text-slate-300">
        {#if value === null}
          —
        {:else}
          {value.toFixed(1)} dBFS
        {/if}
      </span>
    </div>
    <div class="bar">
      <div class="fill" style:width="{fraction * 100}%" style:background-color={colour}></div>
    </div>
    <div class="flex justify-between text-[9px] text-[color:var(--color-muted)]">
      <span>{MIN_DB}</span>
      <span>-45</span>
      <span>0 dBFS</span>
    </div>
  </div>
{/if}

<style>
  .bar {
    position: relative;
    height: 0.5rem;
    border-radius: 0.125rem;
    background: rgb(15 23 42);
    border: 1px solid rgb(30 41 59);
    overflow: hidden;
  }
  .fill {
    position: absolute;
    top: 0;
    left: 0;
    bottom: 0;
    transition: width 60ms linear;
  }
</style>
