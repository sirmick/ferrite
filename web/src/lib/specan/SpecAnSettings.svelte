<script lang="ts">
  // Settings, spectrum-analyzer flavour: the radio input (same controls
  // as the receiver) and the sweep block's set-and-forget knobs, rendered
  // from its BlockSpec like every other block.
  import BlockParams from '$lib/controls/BlockParams.svelte';
  import InputControls from '$lib/controls/InputControls.svelte';
  import { pipeline } from '$lib/pipeline.svelte';

  let src = $derived(pipeline.blocks.src);
  const SWEEP_KEYS = [
    'sweep_start_hz',
    'sweep_stop_hz',
    'rbw_hz',
    'averages',
    'discard_ms',
    'usable_fraction',
    'overlap',
    'dc_mask_hz',
  ];
</script>

<div class="flex h-full w-full flex-col gap-3 overflow-y-auto p-2 text-xs">
  <section>
    <h3 class="mb-1 text-[10px] font-bold uppercase tracking-wider text-[color:var(--color-muted)]">
      Radio input
    </h3>
    <p class="mb-2 text-[10px] text-slate-500">
      The receiver's own radio settings — gain, antenna and the driver's band-stop notches — shared
      as-is; changing one restarts the sweep. AGC is held off while sweeping so levels compare
      across steps, and the sweep runs its own rate ({((src?.values as Record<string, number>)
        ?.sample_rate_hz ?? 0) / 1e6} MS/s) with the widest IF filter.
    </p>
    <InputControls spectrum />
  </section>
  {#if src}
    <section>
      <h3
        class="mb-1 text-[10px] font-bold uppercase tracking-wider text-[color:var(--color-muted)]"
      >
        Sweep ({src.type_name})
      </h3>
      <BlockParams block={src} only={SWEEP_KEYS} />
    </section>
  {/if}
</div>
