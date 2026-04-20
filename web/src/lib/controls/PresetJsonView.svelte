<script lang="ts">
  import { fetchFlowgraph } from '$lib/api/flowgraph';
  import type { FlowgraphDoc } from '$lib/flowgraph';

  interface Props {
    /** Changing this re-fetches; pass the parent dialog's `open` state so
     * the view refreshes every time the tab is surfaced. */
    refreshKey: unknown;
  }

  let { refreshKey }: Props = $props();

  let doc = $state<FlowgraphDoc | null>(null);
  let status = $state<'loading' | 'ready' | 'error'>('loading');
  let errorMessage = $state<string | null>(null);

  $effect(() => {
    // Tie fetch to the refreshKey so the caller controls when we refetch.
    void refreshKey;
    void load();
  });

  async function load() {
    status = 'loading';
    errorMessage = null;
    try {
      doc = await fetchFlowgraph();
      status = 'ready';
    } catch (err) {
      status = 'error';
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  let pretty = $derived(doc ? JSON.stringify(doc, null, 2) : '');
</script>

<div class="flex flex-col gap-2">
  <div class="flex items-center justify-between text-[10px] text-[color:var(--color-muted)]">
    <span>live preset — read-only snapshot</span>
    <button
      type="button"
      class="rounded border border-slate-700 px-2 py-0.5 hover:border-slate-600"
      onclick={() => void load()}
    >
      Refresh
    </button>
  </div>

  {#if status === 'loading'}
    <p class="text-xs text-[color:var(--color-muted)]">Loading…</p>
  {:else if status === 'error'}
    <p class="text-xs text-rose-400">{errorMessage}</p>
  {:else}
    <pre
      class="max-h-[28rem] overflow-auto rounded border border-slate-800 bg-slate-900/60 p-3 font-mono text-[11px] leading-snug text-slate-300">{pretty}</pre>
  {/if}
</div>
