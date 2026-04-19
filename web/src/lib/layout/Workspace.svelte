<script lang="ts">
  import { mount, unmount, type Component } from 'svelte';
  import { onMount } from 'svelte';
  import {
    createDockview,
    type DockviewApi,
    type IContentRenderer,
    type GroupPanelPartInitParameters,
  } from 'dockview-core';
  import 'dockview-core/dist/styles/dockview.css';
  import Spectrum from '$lib/viz/Spectrum.svelte';
  import Waterfall from '$lib/viz/Waterfall.svelte';
  import type { FrameClient } from '$lib/ws/client';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  let container: HTMLDivElement | undefined = $state();
  let api: DockviewApi | undefined;

  // Registry of Svelte components available as dockview panels. The
  // dockview factory dispatches on the panel's `component` name.
  const COMPONENTS: Record<string, Component<{ client: FrameClient }>> = {
    spectrum: Spectrum,
    waterfall: Waterfall,
  };

  class SvelteRenderer implements IContentRenderer {
    private readonly _el = document.createElement('div');
    private instance: Record<string, unknown> | undefined;
    get element(): HTMLElement {
      return this._el;
    }
    init(params: GroupPanelPartInitParameters): void {
      this._el.className = 'h-full w-full';
      const Comp = COMPONENTS[params.params?.component as string];
      if (!Comp) {
        this._el.textContent = `Unknown panel: ${params.api.id}`;
        return;
      }
      this.instance = mount(Comp, { target: this._el, props: { client } });
    }
    dispose(): void {
      if (this.instance) void unmount(this.instance);
      this.instance = undefined;
    }
  }

  onMount(() => {
    if (!container) return;
    api = createDockview(container, {
      createComponent: () => new SvelteRenderer(),
    });

    api.addPanel({
      id: 'spectrum',
      component: 'default',
      title: 'Spectrum',
      params: { component: 'spectrum' },
    });
    api.addPanel({
      id: 'waterfall',
      component: 'default',
      title: 'Waterfall',
      params: { component: 'waterfall' },
      position: { referencePanel: 'spectrum', direction: 'below' },
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
    --dv-tabs-and-actions-container-height: 18px;
    --dv-tabs-and-actions-container-font-size: 9px;
  }
  .dockview-root :global(.tab) {
    font-size: 9px;
    padding: 0 6px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
</style>
