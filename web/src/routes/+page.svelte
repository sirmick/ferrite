<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
</script>

<div class="flex h-dvh w-dvw flex-col">
  <header class="flex items-center justify-between border-b border-slate-800 px-4 py-2">
    <div class="flex items-baseline gap-3">
      <h1 class="text-lg font-semibold">Ferrite</h1>
      <span class="text-xs text-[color:var(--color-muted)]">pre-alpha</span>
    </div>
    <span class="text-xs text-[color:var(--color-muted)]">no device</span>
  </header>
  <div class="min-h-0 flex-1">
    <Workspace />
  </div>
</div>
