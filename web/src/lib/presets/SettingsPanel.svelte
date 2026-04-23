<script lang="ts">
  import { pipeline } from '$lib/pipeline.svelte';
  import BlockParams from '$lib/controls/BlockParams.svelte';
  import InputControls from '$lib/controls/InputControls.svelte';
  import AudioPanel from './AudioPanel.svelte';
  import RssiMeter from '$lib/rssi/RssiMeter.svelte';
  import { RECEIVERS, applyRecipe, detectReceiver, findRecipe, type ReceiverId } from './receivers';

  // Container for everything the user reaches for after the source is
  // open: SDR-side knobs (Input) and demod-side knobs (Receiver).
  // Source/channelizer/FFT/audio plumbing lives in their own surfaces
  // (source dialog, spectrum toolbar, FFT controls strip).
  const DEMOD_ID = 'demod';

  let activeId = $derived(detectReceiver(pipeline.flowgraph));
  let applying = $state(false);
  let demodBlock = $derived(pipeline.blocks[DEMOD_ID]);

  async function swapTo(id: ReceiverId) {
    if (applying || !pipeline.flowgraph) return;
    const recipe = findRecipe(id);
    applying = true;
    try {
      const next = applyRecipe(pipeline.flowgraph, recipe);
      if (next === null) return;
      await pipeline.patchFlowgraph(next);
    } finally {
      applying = false;
    }
  }
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Settings</span>
  </div>

  <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
    <details open class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Input
      </summary>
      <div class="mt-2 flex flex-col gap-3 pl-1">
        <InputControls />
        <RssiMeter />
      </div>
    </details>

    <details open class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Receiver
      </summary>
      <div class="mt-2 flex flex-col gap-3 pl-1">
        <label class="grid gap-1">
          <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
            >Mode</span
          >
          <select
            class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
            value={activeId ?? ''}
            onchange={(e) => {
              const next = (e.currentTarget as HTMLSelectElement).value as ReceiverId;
              void swapTo(next);
            }}
            disabled={applying || pipeline.phase === 'busy'}
          >
            {#if activeId === null}
              <option value="" disabled>— custom —</option>
            {/if}
            {#each RECEIVERS as r (r.id)}
              <option value={r.id}>{r.label}</option>
            {/each}
          </select>
        </label>

        {#if demodBlock}
          <section class="flex flex-col gap-1">
            <header class="flex items-baseline justify-between">
              <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
                >demod</span
              >
              <span class="font-mono text-[10px] text-slate-500">{demodBlock.type_name}</span>
            </header>
            {#if demodBlock.spec.params.length === 0}
              <p class="text-[10px] text-slate-600">no params</p>
            {:else}
              <BlockParams block={demodBlock} hideSourceRestart />
            {/if}
          </section>
        {/if}
      </div>
    </details>

    <details open class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Audio
      </summary>
      <div class="mt-2 pl-1">
        <AudioPanel />
      </div>
    </details>

    {#if pipeline.errorMessage}
      <p class="text-rose-400">{pipeline.errorMessage}</p>
    {/if}
  </div>
</div>

<style>
  .settings-section {
    border-top: 1px solid rgb(30 41 59);
    padding-top: 0.5rem;
  }
  .settings-section:first-child {
    border-top: none;
    padding-top: 0;
  }
</style>
