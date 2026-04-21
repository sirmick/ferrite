<script lang="ts">
  import { pipeline } from '$lib/pipeline.svelte';
  import BlockParams from '$lib/controls/BlockParams.svelte';
  import { RECEIVERS, applyRecipe, detectReceiver, findRecipe, type ReceiverId } from './receivers';

  // The source block carries the sample-rate / gain / frequency knobs
  // that belong in the top-of-spectrum toolbar and the source dialog,
  // not the receiver pane. Everything else in the composed preset is
  // fair game for per-block BlockParams editing here.
  const SOURCE_ID = 'src';

  let activeId = $derived(detectReceiver(pipeline.flowgraph));
  let applying = $state(false);

  let blockList = $derived(
    Object.values(pipeline.blocks)
      .filter((b) => b.id !== SOURCE_ID)
      .sort((a, b) => a.id.localeCompare(b.id)),
  );

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
    <span class="font-semibold text-[color:var(--color-muted)]">Receiver</span>
    <span class="text-[10px] text-[color:var(--color-muted)]">demod + chain knobs</span>
  </div>

  <div class="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-3">
    <label class="grid gap-1">
      <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">Mode</span>
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

    {#each blockList as block (block.id)}
      <section class="flex flex-col gap-1">
        <header class="flex items-baseline justify-between">
          <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
            >{block.id}</span
          >
          <span class="font-mono text-[10px] text-slate-500">{block.type_name}</span>
        </header>
        {#if block.spec.params.length === 0}
          <p class="text-[10px] text-slate-600">no params</p>
        {:else}
          <BlockParams {block} hideSourceRestart />
        {/if}
      </section>
    {/each}

    {#if pipeline.errorMessage}
      <p class="text-rose-400">{pipeline.errorMessage}</p>
    {/if}
  </div>
</div>
