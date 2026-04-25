<script lang="ts">
  // Dual-thumb dB-range slider. Replaces the two `floor` / `ceil` number
  // inputs with a single horizontal track and two draggable thumbs.
  //
  // Layout: left edge of the track is the loudest dB (max, e.g. 0 dBFS),
  // right edge is the quietest (min, e.g. −160 dBFS). The "ceil" thumb
  // therefore sits on the LEFT (it's the upper limit of what's painted)
  // and the "floor" thumb on the RIGHT. The active fill between them
  // visualises the displayed dynamic-range window.
  //
  // Built as a custom div widget rather than two overlapping native
  // ranges because dual-range slider hacks are fiddly to make accessible
  // and the thumbs need pointer-capture for smooth dragging anyway.

  interface Props {
    /** Lower edge of the displayed window (more negative dB). */
    floor: number;
    /** Upper edge of the displayed window (less negative dB). */
    ceil: number;
    /** Inclusive lower bound on either thumb (typically the server
     *  quantisation floor). */
    min?: number;
    /** Inclusive upper bound on either thumb (typically 0 dBFS). */
    max?: number;
    /** Minimum gap between the two thumbs in dB so they don't collapse
     *  into each other and produce a zero-height plot. */
    gap?: number;
    /** Disable both thumbs (e.g. when auto-scale is on). */
    disabled?: boolean;
    /** Fired when either thumb is dragged. The receiver is responsible
     *  for clamping/persisting; the slider only enforces `gap` so the
     *  caller never receives an invalid pair. */
    onChange: (next: { floor: number; ceil: number }) => void;
  }

  let { floor, ceil, min = -160, max = 0, gap = 1, disabled = false, onChange }: Props = $props();

  let trackEl: HTMLDivElement | undefined = $state();
  let dragging = $state<'floor' | 'ceil' | null>(null);

  // Map dB → percentage along the track. Left = max, right = min, so a
  // value of `max` sits at 0% and a value of `min` sits at 100%.
  function dbToPct(db: number): number {
    if (max === min) return 0;
    return ((max - db) / (max - min)) * 100;
  }

  function dbFromClientX(clientX: number): number {
    if (!trackEl) return floor;
    const rect = trackEl.getBoundingClientRect();
    if (rect.width <= 0) return floor;
    const frac = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    return Math.round(max - frac * (max - min));
  }

  function startDrag(which: 'floor' | 'ceil', ev: PointerEvent) {
    if (disabled) return;
    dragging = which;
    (ev.currentTarget as HTMLElement).setPointerCapture(ev.pointerId);
    apply(ev);
  }

  function move(ev: PointerEvent) {
    if (!dragging) return;
    apply(ev);
  }

  function apply(ev: PointerEvent) {
    const db = dbFromClientX(ev.clientX);
    if (dragging === 'ceil') {
      const next = Math.max(floor + gap, Math.min(max, db));
      if (next !== ceil) onChange({ floor, ceil: next });
    } else if (dragging === 'floor') {
      const next = Math.min(ceil - gap, Math.max(min, db));
      if (next !== floor) onChange({ floor: next, ceil });
    }
  }

  function endDrag(ev: PointerEvent) {
    if (!dragging) return;
    dragging = null;
    (ev.currentTarget as HTMLElement).releasePointerCapture(ev.pointerId);
  }

  // Keyboard nudge: arrows shift the focused thumb by ±1 dB,
  // shift+arrow by ±10. Direction matches the visual layout (left
  // arrow = louder, right arrow = quieter), not the underlying numeric.
  function onKey(which: 'floor' | 'ceil', ev: KeyboardEvent) {
    if (disabled) return;
    let delta = 0;
    if (ev.key === 'ArrowLeft')
      delta = +1; // toward max (louder)
    else if (ev.key === 'ArrowRight')
      delta = -1; // toward min (quieter)
    else return;
    if (ev.shiftKey) delta *= 10;
    ev.preventDefault();
    if (which === 'ceil') {
      const next = Math.max(floor + gap, Math.min(max, ceil + delta));
      if (next !== ceil) onChange({ floor, ceil: next });
    } else {
      const next = Math.min(ceil - gap, Math.max(min, floor + delta));
      if (next !== floor) onChange({ floor: next, ceil });
    }
  }

  let ceilPct = $derived(dbToPct(ceil));
  let floorPct = $derived(dbToPct(floor));
</script>

<div
  bind:this={trackEl}
  class="relative h-4 w-32 select-none"
  class:opacity-50={disabled}
  title="display dB range — left thumb = ceiling (loud), right thumb = floor (quiet)"
>
  <!-- Track -->
  <div class="absolute inset-x-0 top-1/2 h-1 -translate-y-1/2 rounded bg-slate-800"></div>
  <!-- Active fill between the two thumbs -->
  <div
    class="absolute top-1/2 h-1 -translate-y-1/2 rounded bg-sky-500/40"
    style:left="{ceilPct}%"
    style:right="{100 - floorPct}%"
  ></div>
  <!-- Ceil thumb (left = loud) -->
  <div
    class="absolute top-1/2 h-3 w-3 -translate-x-1/2 -translate-y-1/2 rounded-full border border-slate-300 bg-slate-100 shadow"
    class:cursor-ew-resize={!disabled}
    style:left="{ceilPct}%"
    role="slider"
    tabindex={disabled ? -1 : 0}
    aria-label="ceiling (max dBFS)"
    aria-valuemin={min}
    aria-valuemax={max}
    aria-valuenow={ceil}
    title="ceiling: {ceil} dBFS"
    onpointerdown={(e) => startDrag('ceil', e)}
    onpointermove={move}
    onpointerup={endDrag}
    onpointercancel={endDrag}
    onkeydown={(e) => onKey('ceil', e)}
  ></div>
  <!-- Floor thumb (right = quiet) -->
  <div
    class="absolute top-1/2 h-3 w-3 -translate-x-1/2 -translate-y-1/2 rounded-full border border-slate-300 bg-slate-100 shadow"
    class:cursor-ew-resize={!disabled}
    style:left="{floorPct}%"
    role="slider"
    tabindex={disabled ? -1 : 0}
    aria-label="floor (min dBFS)"
    aria-valuemin={min}
    aria-valuemax={max}
    aria-valuenow={floor}
    title="floor: {floor} dBFS"
    onpointerdown={(e) => startDrag('floor', e)}
    onpointermove={move}
    onpointerup={endDrag}
    onpointercancel={endDrag}
    onkeydown={(e) => onKey('floor', e)}
  ></div>
</div>
