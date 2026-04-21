<script lang="ts">
  // Two-pane resizable splitter. `direction` is the *layout axis* — 'row'
  // splits left/right with a vertical drag bar, 'column' splits top/bottom
  // with a horizontal drag bar. The bar is a thin pointer-event target
  // with a wider invisible hit area so it's grabbable without being
  // visually loud. Fraction (`a`'s share) is stored locally and persisted
  // to localStorage when `storageKey` is provided.
  import type { Snippet } from 'svelte';
  import { onMount } from 'svelte';

  interface Props {
    direction: 'row' | 'column';
    a: Snippet;
    b: Snippet;
    defaultFraction?: number;
    min?: number;
    max?: number;
    storageKey?: string;
  }

  let {
    direction,
    a,
    b,
    defaultFraction = 0.5,
    min = 0.1,
    max = 0.9,
    storageKey,
  }: Props = $props();

  // svelte-ignore state_referenced_locally
  // defaultFraction is intentionally one-shot — once mounted, the
  // fraction is owned by user drags + localStorage.
  let fraction = $state(defaultFraction);
  let container: HTMLDivElement | undefined = $state();
  let dragging = $state(false);

  onMount(() => {
    if (!storageKey) return;
    const raw = localStorage.getItem(storageKey);
    if (raw === null) return;
    const v = Number(raw);
    if (Number.isFinite(v) && v >= min && v <= max) fraction = v;
  });

  $effect(() => {
    if (!storageKey) return;
    localStorage.setItem(storageKey, String(fraction));
  });

  function onPointerDown(ev: PointerEvent) {
    if (ev.button !== 0 || !container) return;
    (ev.currentTarget as HTMLElement).setPointerCapture(ev.pointerId);
    dragging = true;
    ev.preventDefault();
  }

  function onPointerMove(ev: PointerEvent) {
    if (!dragging || !container) return;
    const rect = container.getBoundingClientRect();
    const raw =
      direction === 'row'
        ? (ev.clientX - rect.left) / rect.width
        : (ev.clientY - rect.top) / rect.height;
    fraction = Math.max(min, Math.min(max, raw));
  }

  function onPointerUp(ev: PointerEvent) {
    if (!dragging) return;
    (ev.currentTarget as HTMLElement).releasePointerCapture(ev.pointerId);
    dragging = false;
  }

  let aSize = $derived(`${(fraction * 100).toFixed(3)}%`);
  let bSize = $derived(`${((1 - fraction) * 100).toFixed(3)}%`);
</script>

<div
  bind:this={container}
  class="split"
  class:row={direction === 'row'}
  class:column={direction === 'column'}
>
  <div
    class="pane"
    style:flex-basis={aSize}
    style:width={direction === 'row' ? aSize : undefined}
    style:height={direction === 'column' ? aSize : undefined}
  >
    {@render a()}
  </div>
  <div
    class="bar"
    class:dragging
    role="separator"
    aria-orientation={direction === 'row' ? 'vertical' : 'horizontal'}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
  ></div>
  <div
    class="pane"
    style:flex-basis={bSize}
    style:width={direction === 'row' ? bSize : undefined}
    style:height={direction === 'column' ? bSize : undefined}
  >
    {@render b()}
  </div>
</div>

<style>
  .split {
    display: flex;
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
  }
  .split.row {
    flex-direction: row;
  }
  .split.column {
    flex-direction: column;
  }
  .pane {
    min-width: 0;
    min-height: 0;
    overflow: hidden;
    flex-grow: 0;
    flex-shrink: 0;
  }
  .bar {
    position: relative;
    flex: 0 0 1px;
    background: rgb(30 41 59);
    z-index: 5;
  }
  .row > .bar {
    cursor: col-resize;
    width: 1px;
  }
  .column > .bar {
    cursor: row-resize;
    height: 1px;
  }
  /* widen the hit target without taking up layout space */
  .bar::after {
    content: '';
    position: absolute;
    inset: 0;
  }
  .row > .bar::after {
    left: -3px;
    right: -3px;
  }
  .column > .bar::after {
    top: -3px;
    bottom: -3px;
  }
  .bar:hover,
  .bar.dragging {
    background: rgb(56 189 248);
  }
</style>
