<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import { demoAddInWorker } from '$lib/workers/demo-client';

  let wasmStatus = $state<'pending' | 'ok' | string>('pending');

  async function runDemo() {
    try {
      const { sum } = await demoAddInWorker(1.5, 2.25);
      wasmStatus = Math.abs(sum - 3.75) < 1e-6 ? 'ok' : `wrong: ${sum}`;
    } catch (err) {
      wasmStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  $effect(() => {
    void runDemo();
  });
</script>

<div class="flex h-dvh w-dvw flex-col">
  <header class="flex items-center justify-between border-b border-slate-800 px-4 py-2">
    <div class="flex items-baseline gap-3">
      <h1 class="text-lg font-semibold">Ferrite</h1>
      <span class="text-xs text-[color:var(--color-muted)]">pre-alpha</span>
    </div>
    <div class="flex items-center gap-4 text-xs text-[color:var(--color-muted)]">
      <span>wasm: {wasmStatus}</span>
      <span>no device</span>
    </div>
  </header>
  <div class="min-h-0 flex-1">
    <Workspace />
  </div>
</div>
