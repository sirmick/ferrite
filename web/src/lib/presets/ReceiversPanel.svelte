<script lang="ts">
  import { fetchFlowgraph, patchFlowgraph, type ReconfigurePlan } from '$lib/api/flowgraph';
  import { RECEIVERS, applyRecipe, detectReceiver, findRecipe, type ReceiverId } from './receivers';
  import { onMount } from 'svelte';

  let status = $state<'idle' | 'loading' | 'not_preset' | 'error' | 'ready' | 'applying'>('idle');
  let errorMessage = $state<string | null>(null);
  let activeId = $state<ReceiverId | null>(null);
  let lastPlan = $state<ReconfigurePlan | null>(null);

  onMount(() => {
    void refresh();
  });

  async function refresh() {
    status = 'loading';
    errorMessage = null;
    try {
      const doc = await fetchFlowgraph();
      if (doc === null) {
        status = 'not_preset';
        activeId = null;
        return;
      }
      activeId = detectReceiver(doc);
      status = 'ready';
    } catch (err) {
      status = 'error';
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  async function swapTo(id: ReceiverId) {
    if (status === 'applying') return;
    const recipe = findRecipe(id);
    status = 'applying';
    errorMessage = null;
    try {
      const doc = await fetchFlowgraph();
      if (doc === null) {
        status = 'not_preset';
        return;
      }
      const next = applyRecipe(doc, recipe);
      if (next === null) {
        status = 'error';
        errorMessage = 'live preset has no demod block — cannot swap receiver';
        return;
      }
      const plan = await patchFlowgraph(next);
      lastPlan = plan;
      activeId = id;
      status = 'ready';
    } catch (err) {
      status = 'error';
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Receivers</span>
    <span class="text-[10px] text-[color:var(--color-muted)]">swap demod chain</span>
  </div>

  <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
    {#if status === 'loading' || status === 'idle'}
      <p class="text-[color:var(--color-muted)]">Loading…</p>
    {:else if status === 'not_preset'}
      <p class="text-[color:var(--color-muted)]">
        ferrited is not in preset mode; start with <code class="font-mono">--flowgraph</code> to use receivers.
      </p>
    {:else}
      <label class="grid gap-1">
        <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">Mode</span
        >
        <select
          class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
          value={activeId ?? ''}
          onchange={(e) => {
            const next = (e.currentTarget as HTMLSelectElement).value as ReceiverId;
            void swapTo(next);
          }}
          disabled={status === 'applying'}
        >
          {#if activeId === null}
            <option value="" disabled>— custom —</option>
          {/if}
          {#each RECEIVERS as r (r.id)}
            <option value={r.id}>{r.label}</option>
          {/each}
        </select>
      </label>

      {#if status === 'applying'}
        <p class="text-[color:var(--color-muted)]">Applying…</p>
      {:else if lastPlan}
        <p class="text-[11px] text-emerald-400">
          {lastPlan.noop ? 'no-op' : lastPlan.overall} — {lastPlan.changes.length} param change{lastPlan
            .changes.length === 1
            ? ''
            : 's'}, {lastPlan.structural_count} structural
        </p>
      {/if}
    {/if}

    {#if status === 'error'}
      <p class="text-rose-400">{errorMessage}</p>
    {/if}
  </div>
</div>
