<script lang="ts">
  // Spectrum-analyzer workspace: same toolbar as the receiver, its own
  // quick bar, and one content area (the panorama).
  import AppToolbar from '$lib/layout/AppToolbar.svelte';
  import ViewBridge from '$lib/layout/ViewBridge.svelte';
  import SpecAnQuickBar from './SpecAnQuickBar.svelte';
  import Panorama from './Panorama.svelte';
</script>

<div class="flex h-full min-h-0 w-full flex-col">
  <ViewBridge />
  <header class="toolbar-row">
    <AppToolbar />
  </header>
  <SpecAnQuickBar />
  <div class="min-h-0 flex-1">
    <Panorama />
  </div>
</div>

<style>
  .toolbar-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 7px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    background: var(--color-bg);
  }
</style>
