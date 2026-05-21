// Shared click-and-drag-to-tune coordinator for the spectrum / waterfall
// panes (wide + narrow). Handles the click-vs-drag distinction,
// rAF-throttling, pointer capture, and axis snapshotting — the per-pane
// `onClick` / `onDrag` callbacks decide what the gesture actually does.
//
// Two drag flavours live in the codebase, both via this same factory:
//
//   - VFO drag (narrow panes): each move sets `chan.freq_shift_hz` so
//     the VFO lands under the cursor — fine-tune inside the channel.
//
//   - Pan drag (wide panes): each move shifts `flow.src.center_freq_hz`
//     so the freq under the cursor at drag-start follows the cursor;
//     the VFO marker stays at the same pixel and the source LO slides,
//     which in turn moves the VFO's absolute freq (freq_shift stays put).
//
// In both cases a pure click (pointer-down/up with no movement) routes
// through `onClick` — typically `tuneVfoExact` so the per-driver DC
// dodge still kicks in. Snap-to-grid is intentionally skipped on both
// paths: the operator pointed at a precise pixel.
//
// Narrow views centre on the VFO, so re-reading the axis each move
// would let the drag chase itself; the axis is frozen at pointer-down
// and held for the gesture. Wide views don't re-centre, but the same
// snapshot semantics also gives pan drag a stable "start LO" reference.

import { vfoState } from './tuning.svelte';

/** Axis metadata for a canvas-based pane. The freq window is the
 *  rectangle of spectrum painted between `marginLeftPx` and
 *  `canvasWidth - marginRightPx` — the spectrum renderer reserves
 *  pixels at the canvas edges for axis labels (LEFT_MARGIN=44,
 *  RIGHT_MARGIN=6) while the waterfall renderers paint edge-to-edge
 *  inside a padded wrapper. Margins must travel with the axis so
 *  pixel→Hz lands on the correct frequency. */
export interface PaneAxis {
  centerHz: number;
  rateHz: number;
  marginLeftPx?: number;
  marginRightPx?: number;
}

/** Per-move context passed to `onDrag`. Everything is captured at
 *  pointer-down (axis, canvasWidth, startCursorX) or carried fresh from
 *  the current event (cursorX). `targetHz` is the freq under the
 *  cursor right now, pre-computed against the captured axis (margins
 *  honoured) so the callback doesn't redo the pixel→Hz math. */
export interface DragContext {
  /** Canvas-relative pointer X for this move. */
  cursorX: number;
  /** Canvas-relative pointer X captured at pointer-down. */
  startCursorX: number;
  /** Freq under the current cursor position, computed against the
   *  captured axis (margin-aware). */
  targetHz: number;
  /** Freq window the canvas was painting at pointer-down. */
  axis: PaneAxis;
  /** Canvas client width at pointer-down (in CSS pixels). */
  canvasWidth: number;
}

export interface PointerTuneOpts {
  /** The canvas the pointer is interacting with. Read lazily so the
   *  helper survives re-mounts and HMR. */
  getCanvas(): HTMLCanvasElement | undefined;
  /** Current freq window painted by the canvas, plus the renderer's
   *  axis-label margins so pixel→Hz uses the plot's interior width.
   *  Read at pointer-down and held for the gesture. */
  getAxis(): PaneAxis | undefined;
  /** Single click (pointer up after a `down`/`up` with no movement, and
   *  no second `down` within the dblclick window). Use for the
   *  "everyday tune" action — typically VFO-only via `dragVfoExact` so
   *  the source LO stays parked. */
  onClick(absHz: number): void;
  /** Double click (two `up`s in a row, neither a drag, within the
   *  dblclick window). Use for the heavier "re-acquire" action —
   *  typically `pipeline.tune` so the per-driver DC dodge can move the
   *  source LO if needed. */
  onDoubleClick(absHz: number): void;
  /** Each settled drag step. Callers MUST `return` (or `await`) the
   *  `applyControl` call inside so the factory's in-flight gate sees
   *  when the server has acknowledged — see the comment on `inFlight`
   *  in `createPointerTune`. */
  onDrag(ctx: DragContext): Promise<void> | void;
  /** Notified on `dragging` transitions so callers can flip cursor
   *  styling / overlay state. */
  onDragChange?(dragging: boolean): void;
  /** Called on every pointer-over with the freq under the cursor
   *  (computed against the live, margin-aware axis), and `null` when
   *  the pointer leaves the canvas. Drives the cross-pane VFO-preview
   *  line via `hoverStore`. Skipped while dragging — during a drag the
   *  VFO marker is already chasing the cursor, so a separate preview
   *  line would just overlap. */
  onHover?(absHz: number | null): void;
}

export interface PointerTuneHandlers {
  onpointerdown(ev: PointerEvent): void;
  onpointermove(ev: PointerEvent): void;
  onpointerup(ev: PointerEvent): void;
  onpointercancel(ev: PointerEvent): void;
  onpointerleave(ev: PointerEvent): void;
}

/** Build a fresh pointer-tune handler set bound to the supplied opts.
 *  Each pane keeps its own instance — state (drag in-flight, pending
 *  ctx, axis snapshot, start cursor) lives in the closure.
 *
 *  Drag pacing is *settled-based*, not rAF-throttled: pointer moves
 *  only update `pendingCtx`, and the in-flight gate fires the next
 *  drag step when the previous one resolves. Intermediate values are
 *  coalesced — only the most recent ctx survives. This adapts to the
 *  server's actual throughput. A naive rAF (60 Hz) fired
 *  `applyControl` calls faster than the browser's 6-per-host
 *  connection limit could drain them, so the queue built up during
 *  the gesture and flushed *after* pointer-up — visible as the
 *  spectrum scrolling on release rather than during the drag. */
/** Pointer-up to second-pointer-down threshold for the dblclick
 *  detection. 250 ms is the conventional web-app dblclick window —
 *  long enough to catch deliberate double-taps, short enough not to
 *  feel like single-click is lagging. */
const DBLCLICK_MS = 250;

export function createPointerTune(opts: PointerTuneOpts): PointerTuneHandlers {
  let dragging = false;
  let dragMoved = false;
  let pendingCtx: DragContext | null = null;
  // Deferred single-click: fires after DBLCLICK_MS if no second click
  // arrives. Cancelled on a second pointer-down within the window so
  // the dblclick action can take its place.
  let pendingClickHz: number | null = null;
  let pendingClickTimer: ReturnType<typeof setTimeout> | null = null;
  // True while a drag step is mid-flight (awaiting the server). The
  // settled handler picks up `pendingCtx` (if any) when the in-flight
  // promise resolves, so the drag stays at one round-trip deep no
  // matter how fast the pointer moves.
  let inFlight = false;
  // Frozen at pointer-down. axisSnap.centerHz is the freq window
  // centre (= source LO for wide panes, VFO abs for narrow panes) at
  // gesture start; pan-mode drag pivots around this.
  let axisSnap: PaneAxis | undefined;
  let startCursorX = 0;
  let canvasWidth = 0;

  function canvasXOf(ev: PointerEvent, c: HTMLCanvasElement): number {
    return ev.clientX - c.getBoundingClientRect().left;
  }

  function hzAt(cursorX: number): number | undefined {
    if (!axisSnap || !(axisSnap.rateHz > 0) || !(canvasWidth > 0)) return undefined;
    return hzFromAxis(cursorX, canvasWidth, axisSnap);
  }

  /** Margin-aware pixel→Hz against an arbitrary (axis, width) — used
   *  by the hover path before any axis snapshot exists. */
  function hzFromAxis(cursorX: number, widthPx: number, axis: PaneAxis): number {
    const plotLeft = axis.marginLeftPx ?? 0;
    const plotW = Math.max(1, widthPx - plotLeft - (axis.marginRightPx ?? 0));
    const frac = (cursorX - plotLeft) / plotW;
    return axis.centerHz - axis.rateHz / 2 + frac * axis.rateHz;
  }

  /** Drain `pendingCtx` while honouring the in-flight gate. Each
   *  iteration takes the latest pending ctx (intermediate ctxs are
   *  dropped) and awaits the caller's onDrag — usually `applyControl`,
   *  which resolves only after the server reconfigure lands. */
  async function drainPending() {
    if (inFlight) return; // a previous drainer is already iterating
    inFlight = true;
    try {
      while (pendingCtx !== null) {
        const ctx = pendingCtx;
        pendingCtx = null;
        await opts.onDrag(ctx);
      }
    } finally {
      inFlight = false;
    }
  }

  function endDrag() {
    if (!dragging) return;
    dragging = false;
    axisSnap = undefined;
    opts.onDragChange?.(false);
  }

  function firePendingClick() {
    if (pendingClickHz === null) return;
    const hz = pendingClickHz;
    pendingClickHz = null;
    pendingClickTimer = null;
    opts.onClick(hz);
  }

  function cancelPendingClick() {
    if (pendingClickTimer !== null) {
      clearTimeout(pendingClickTimer);
      pendingClickTimer = null;
    }
    pendingClickHz = null;
  }

  function onpointerdown(ev: PointerEvent) {
    if (ev.button !== 0) return;
    if (!vfoState()) return; // bareband preset — nothing to tune
    const c = opts.getCanvas();
    if (!c) return;
    const axis = opts.getAxis();
    if (!axis) return;
    // Second-click-in-progress detection: a pointer-down while the
    // dblclick timer is still pending means "this is the second click".
    // Stop the timer so the deferred single-click doesn't fire — but
    // leave pendingClickHz non-null so the matching pointer-up knows
    // to treat itself as a dblclick.
    if (pendingClickTimer !== null) {
      clearTimeout(pendingClickTimer);
      pendingClickTimer = null;
    }
    c.setPointerCapture(ev.pointerId);
    dragging = true;
    dragMoved = false;
    axisSnap = axis;
    canvasWidth = c.getBoundingClientRect().width;
    startCursorX = canvasXOf(ev, c);
    opts.onDragChange?.(true);
    // Clear hover preview — the VFO marker will chase the cursor
    // through dragVfoExact, so a separate hover line is redundant.
    opts.onHover?.(null);
  }

  function onpointermove(ev: PointerEvent) {
    const c = opts.getCanvas();
    if (!c) return;
    if (dragging && axisSnap) {
      const cursorX = canvasXOf(ev, c);
      const targetHz = hzAt(cursorX);
      if (targetHz === undefined) return;
      dragMoved = true;
      pendingCtx = { cursorX, startCursorX, targetHz, axis: axisSnap, canvasWidth };
      void drainPending();
      return;
    }
    // Hover path: not dragging → publish cursor freq for the shared
    // preview line. Use the live axis (the snapshot only exists
    // mid-drag) and the canvas's current width.
    if (opts.onHover === undefined) return;
    const axis = opts.getAxis();
    if (!axis) return;
    const rect = c.getBoundingClientRect();
    const hz = hzFromAxis(ev.clientX - rect.left, rect.width, axis);
    opts.onHover(Number.isFinite(hz) ? hz : null);
  }

  function onpointerup(ev: PointerEvent) {
    if (!dragging) return;
    const c = opts.getCanvas();
    c?.releasePointerCapture(ev.pointerId);
    const cursorX = c ? canvasXOf(ev, c) : startCursorX;
    const wasMoved = dragMoved;
    // Capture the final ctx + click freq BEFORE endDrag() releases
    // the snapshot.
    const finalTargetHz = hzAt(cursorX);
    const finalCtx: DragContext | null =
      axisSnap && finalTargetHz !== undefined
        ? { cursorX, startCursorX, targetHz: finalTargetHz, axis: axisSnap, canvasWidth }
        : null;
    const clickHz = !wasMoved ? finalTargetHz : undefined;
    endDrag();
    if (wasMoved) {
      // Drag landed — drain final position. A drag cancels any
      // dblclick-in-progress (drag is its own gesture).
      cancelPendingClick();
      if (finalCtx) {
        pendingCtx = finalCtx;
        void drainPending();
      }
      return;
    }
    if (clickHz === undefined) return;
    // No-drag pointer-up. Two possibilities:
    //   - this is the second click in a dblclick (pendingClickHz is set
    //     by the *previous* up; the second `down` cleared its timer
    //     without nulling pendingClickHz, signalling "fire dblclick on
    //     next up") → fire onDoubleClick.
    //   - this is the first click → defer for DBLCLICK_MS in case a
    //     second click arrives.
    if (pendingClickHz !== null && pendingClickTimer === null) {
      pendingClickHz = null;
      // Use the second click's freq — more accurate than the first
      // since the cursor may have shifted between the two taps.
      opts.onDoubleClick(clickHz);
      return;
    }
    pendingClickHz = clickHz;
    pendingClickTimer = setTimeout(firePendingClick, DBLCLICK_MS);
  }

  function onpointercancel(ev: PointerEvent) {
    if (!dragging) return;
    const c = opts.getCanvas();
    c?.releasePointerCapture(ev.pointerId);
    endDrag();
    // Drop any pending step — gesture was abandoned, no final position.
    pendingCtx = null;
    cancelPendingClick();
    opts.onHover?.(null);
  }

  function onpointerleave(_ev: PointerEvent) {
    // Pointer left the canvas — clear our contribution to the hover
    // preview. Dragging continues uninterrupted (pointer capture
    // routes the events back here either way).
    if (!dragging) opts.onHover?.(null);
  }

  return { onpointerdown, onpointermove, onpointerup, onpointercancel, onpointerleave };
}
