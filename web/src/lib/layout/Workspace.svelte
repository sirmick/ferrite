<script lang="ts">
  import { onMount } from 'svelte';
  import {
    createDockview,
    type DockviewApi,
    type IContentRenderer,
    type GroupPanelPartInitParameters,
  } from 'dockview-core';
  import 'dockview-core/dist/styles/dockview.css';

  let container: HTMLDivElement | undefined;
  let api: DockviewApi | undefined;

  class PlaceholderRenderer implements IContentRenderer {
    private readonly _el = document.createElement('div');
    get element(): HTMLElement {
      return this._el;
    }
    init(params: GroupPanelPartInitParameters): void {
      this._el.className = 'h-full w-full p-4 text-sm text-[color:var(--color-muted)]';
      this._el.textContent = `Panel: ${params.title ?? params.api.id}`;
    }
  }

  onMount(() => {
    if (!container) return;
    api = createDockview(container, {
      createComponent: () => new PlaceholderRenderer(),
    });

    api.addPanel({
      id: 'waterfall',
      component: 'default',
      title: 'Waterfall',
    });

    return () => {
      api?.dispose();
    };
  });
</script>

<div bind:this={container} class="dockview-root dockview-theme-abyss h-full w-full"></div>

<style>
  .dockview-root {
    --dv-background-color: var(--color-bg);
    --dv-paneview-active-outline-color: var(--color-accent);
  }
</style>
