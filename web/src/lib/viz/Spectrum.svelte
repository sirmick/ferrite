<script lang="ts">
  import { onMount } from 'svelte';
  import { LEFT_MARGIN, RIGHT_MARGIN, SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { dragVfoExact, stepVfo, tuneVfoExact } from '$lib/control/tuning.svelte';
  import { createPointerTune } from '$lib/control/pointerTune';
  import { hoverStore, hoverPctInWindow } from './hoverStore.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { bandplanUsa } from '$lib/presets/bandplan';
  import { waterfallStore } from './waterfallStore.svelte';
  import { registerView, unregisterView, dataUrlToBase64 } from './viewRegistry';
  import { signals } from '$lib/signals/store.svelte';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  // stream_id for the preset's `ui:fft` sink — env_split allocates it
  // from 1000+ in doc order, so the server tells us which one to
  // subscribe to rather than the client guessing.
  let fftStreamId = $derived(pipeline.uiSinks.fft?.stream_id);

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: SpectrumRenderer | undefined;

  // The FFT is tapped directly off the source (standard SDR UX — see
  // wbfm.json), so the renderer labels match `currentAxes`. Clicks
  // inside the wide view move the VFO (orange marker) to the click
  // location; the source itself stays put unless there's no VFO.
  let axes = $derived(currentAxes(pipeline));

  // The "VFO block" is whichever block in the composed preset exposes a
  // `freq_shift_hz` param — that's the channelizer baseband-shift knob
  // today. When present, it drives the orange Nixie; absent, we hide
  // the VFO controls so bareband presets stay uncluttered.
  let vfoBlock = $derived(
    Object.values(pipeline.blocks).find((b) =>
      b.spec.params.some((p) => p.key === 'freq_shift_hz'),
    ),
  );
  let vfoValues = $derived(
    (vfoBlock?.values as Record<string, unknown> | null | undefined) ?? null,
  );
  let vfoShiftHz = $derived(
    typeof vfoValues?.freq_shift_hz === 'number' ? vfoValues.freq_shift_hz : 0,
  );
  let vfoAbsHz = $derived((axes?.center_freq_hz ?? 0) + vfoShiftHz);
  // Channel bandwidth shown as a translucent band around the VFO on the
  // FFT — the slice of spectrum being demodulated. Prefer the explicit
  // `output_rate_hz` from the preset; fall back to Fs_in / factor when a
  // preset drives the channelizer by decimation instead.
  let vfoWidthHz = $derived.by(() => {
    if (!vfoValues || !axes) return undefined;
    const outRate = vfoValues.output_rate_hz;
    if (typeof outRate === 'number' && outRate > 0) return outRate;
    const factor = vfoValues.factor;
    if (typeof factor === 'number' && factor > 0) return axes.sample_rate_hz / factor;
    return undefined;
  });

  // Wheel pans the zoomed view across the source span — a fraction of
  // the available headroom per notch. The nixies handle precision tuning
  // and the SDR centre is the user's job; wheel here is purely a
  // navigation aid for the visible window.
  const WHEEL_PAN_FRACTION = 0.05;

  // Display toggles live in the client control store so they persist
  // across reloads and stay read/write through the same `applyControl`
  // dispatch path every other knob uses.
  let fade = $derived(clientControls.get('client.spectrum.fade'));
  let maxHold = $derived(clientControls.get('client.spectrum.maxHold'));
  let autoScale = $derived(clientControls.get('client.spectrum.autoScale'));
  let bandPlan = $derived(clientControls.get('client.spectrum.bandPlan'));
  let viewZoom = $derived(clientControls.get('client.spectrum.viewZoom'));
  let viewPan = $derived(clientControls.get('client.spectrum.viewPan'));

  // Display-only view window. At zoom 1 the renderer paints the full
  // axes span (no-op); at higher zoom we compute the visible centre by
  // sliding `viewPan` ∈ [0,1] across the available headroom of the
  // span. The renderer clamps internally too — this just keeps the UI
  // and the click-to-tune projection in sync without a round-trip.
  let viewWindow = $derived.by(() => {
    if (!axes || !Number.isFinite(axes.sample_rate_hz) || axes.sample_rate_hz <= 0)
      return undefined;
    // Sanitise persisted view params: a non-finite zoom/pan must never
    // reach the renderer (a NaN span makes its grid loop never
    // terminate — frozen tab).
    const z = Number.isFinite(viewZoom) ? Math.max(1, viewZoom) : 1;
    if (z <= 1) return undefined;
    const pan = Number.isFinite(viewPan) ? Math.max(0, Math.min(1, viewPan)) : 0.5;
    const span = axes.sample_rate_hz / z;
    const headroom = axes.sample_rate_hz - span;
    const fullMin = axes.center_freq_hz - axes.sample_rate_hz / 2;
    const viewMin = fullMin + pan * headroom;
    return { centerHz: viewMin + span / 2, rateHz: span };
  });

  // VFO commit routes through the central snap-aware path (which owns
  // the abs→freq_shift split + span clamp + DC-dodge composition).

  const ZOOM_STEP = 1.3;
  const MAX_ZOOM = 64;
  const clamp01 = (x: number) => Math.max(0, Math.min(1, x));

  /** Zoom by `factor` around the current view's centre frequency.
   *  Used by the overlay +/− buttons; the wheel handler anchors at
   *  the cursor instead. Centring on the visible midpoint means a
   *  button click changes the span around the part of the band the
   *  operator is already looking at. */
  function zoomBy(factor: number) {
    if (!axes) return;
    const rate = axes.sample_rate_hz;
    const zOld = Math.max(1, viewZoom);
    const zNew = Math.max(1, Math.min(MAX_ZOOM, zOld * factor));
    if (zNew === zOld) return;
    const fullMin = axes.center_freq_hz - rate / 2;
    const spanOld = rate / zOld;
    const headOld = rate - spanOld;
    const viewCenterFreq = fullMin + clamp01(viewPan) * headOld + spanOld / 2;
    const spanNew = rate / zNew;
    const headNew = rate - spanNew;
    const panNew = headNew > 0 ? clamp01((viewCenterFreq - spanNew / 2 - fullMin) / headNew) : 0.5;
    if (zNew !== viewZoom) void applyControl('client.spectrum.viewZoom', zNew);
    if (panNew !== viewPan) void applyControl('client.spectrum.viewPan', panNew);
  }

  function resetZoom() {
    if (viewZoom !== 1) void applyControl('client.spectrum.viewZoom', 1);
    if (viewPan !== 0.5) void applyControl('client.spectrum.viewPan', 0.5);
  }

  // Wheel = zoom about the cursor: the frequency under the pointer
  // stays under the pointer as the span grows/shrinks (so it's a
  // combined zoom + view shift, not a separate pan gesture).
  function onWheel(ev: WheelEvent) {
    if (!axes || !renderer || !canvas) return;
    ev.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const fAnchorRaw = renderer.pixelToFreq(ev.clientX - rect.left);

    const rate = axes.sample_rate_hz;
    const fullMin = axes.center_freq_hz - rate / 2;
    const zOld = Math.max(1, viewZoom);
    const zNew = Math.max(
      1,
      Math.min(MAX_ZOOM, zOld * (ev.deltaY < 0 ? ZOOM_STEP : 1 / ZOOM_STEP)),
    );
    if (zNew === zOld) return;

    const spanOld = rate / zOld;
    const headOld = rate - spanOld;
    const viewMinOld = fullMin + clamp01(viewPan) * headOld;
    const fAnchor =
      fAnchorRaw !== undefined && Number.isFinite(fAnchorRaw)
        ? fAnchorRaw
        : viewMinOld + spanOld / 2;
    const frac = spanOld > 0 ? (fAnchor - viewMinOld) / spanOld : 0.5;

    const spanNew = rate / zNew;
    const headNew = rate - spanNew;
    const panNew = headNew > 0 ? clamp01((fAnchor - frac * spanNew - fullMin) / headNew) : 0.5;

    if (zNew !== viewZoom) void applyControl('client.spectrum.viewZoom', zNew);
    if (panNew !== viewPan) void applyControl('client.spectrum.viewPan', panNew);
  }

  // Up/Down step the VFO; Left/Right pan the zoomed view. Bound to the
  // canvas (focusable) so it never collides with the Nixie's
  // per-digit arrow handling.
  function onKeydown(ev: KeyboardEvent) {
    switch (ev.key) {
      case 'ArrowUp':
        ev.preventDefault();
        stepVfo(1);
        return;
      case 'ArrowDown':
        ev.preventDefault();
        stepVfo(-1);
        return;
      case 'ArrowLeft':
      case 'ArrowRight': {
        if (viewZoom <= 1) return;
        ev.preventDefault();
        const sign = ev.key === 'ArrowLeft' ? -1 : 1;
        const next = clamp01(viewPan + sign * WHEEL_PAN_FRACTION);
        if (next !== viewPan) void applyControl('client.spectrum.viewPan', next);
        return;
      }
    }
  }

  // Gesture grammar (same across wide + narrow panes):
  //   - single click: VFO-only tune (source LO parked) → dragVfoExact
  //   - double click: full tune via /api/tune (per-driver DC dodge can
  //     move the source LO) → tuneVfoExact
  //   - drag: live VFO-only retune, source LO untouched
  // Snap is ignored on all three — the cursor pointed at a pixel.
  let dragging = $state(false);
  const ptr = createPointerTune({
    getCanvas: () => canvas,
    getAxis: () =>
      axes
        ? {
            centerHz: viewWindow?.centerHz ?? axes.center_freq_hz,
            rateHz: viewWindow?.rateHz ?? axes.sample_rate_hz,
            // The SpectrumRenderer paints axis labels inside the canvas
            // at the left/right edges; click → Hz must use only the
            // plot's interior width or every tune lands ~5 % off.
            marginLeftPx: LEFT_MARGIN,
            marginRightPx: RIGHT_MARGIN,
          }
        : undefined,
    onClick: (hz) => {
      void dragVfoExact(hz);
    },
    onDoubleClick: (hz) => tuneVfoExact(hz),
    onDrag: ({ targetHz }) => dragVfoExact(targetHz),
    onDragChange: (d) => (dragging = d),
    onHover: (hz) => (hoverStore.freqHz = hz),
  });

  // Cross-pane hover preview. Pct is null when the hovered freq sits
  // outside this pane's currently-visible window.
  let hoverPct = $derived(
    hoverPctInWindow(
      hoverStore.freqHz,
      viewWindow?.centerHz ?? axes?.center_freq_hz,
      viewWindow?.rateHz ?? axes?.sample_rate_hz,
    ),
  );

  onMount(() => {
    if (!canvas) return;
    renderer = new SpectrumRenderer(canvas);
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    // The "reset" button lives in DisplayControls (sibling of the
    // waterfall pane); install a renderer-bound impl on the shared
    // store so a click reaches our renderer here. Cleared on teardown.
    waterfallStore.resetMaxHold = () => renderer?.resetMaxHold();
    // Register this canvas for `ferrite-ctl view wide-spectrum`. The
    // snapshot fn is captured here so it closes over the local
    // `canvas` ref — module-level registry stays decoupled from any
    // particular Svelte instance.
    const snapshot = () => dataUrlToBase64(canvas!.toDataURL('image/png'));
    registerView('wide-spectrum', snapshot);
    return () => {
      unregisterView('wide-spectrum', snapshot);
      waterfallStore.resetMaxHold = () => {};
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  $effect(() => {
    const sid = fftStreamId;
    if (sid === undefined) return;
    const unsub = client.subscribe(sid, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      renderer?.setRow(frame.payload);
    });
    return unsub;
  });

  // Server now quantises to a fixed [−160, 0] dBFS window (see
  // LogMagU8::SERVER_FLOOR_DBFS / _CEIL_DBFS); byte 0 = −160 dBFS,
  // byte 255 = 0 dBFS. That's the renderer's absolute reference for
  // unmapping bytes to dB. Any "display zoom" sits on top via the
  // display-range override (`client.spectrum.displayFloorDbfs`/
  // `displayCeilDbfs`) — see the auto-scale effect below.
  const SERVER_FLOOR_DBFS = -160;
  const SERVER_CEIL_DBFS = 0;
  $effect(() => {
    if (!renderer) return;
    if (!axes) {
      renderer.setAxes(undefined);
      return;
    }
    renderer.setAxes({
      centerHz: axes.center_freq_hz,
      rateHz: axes.sample_rate_hz,
      floorDbfs: SERVER_FLOOR_DBFS,
      ceilDbfs: SERVER_CEIL_DBFS,
    });
  });

  // Manual display range from the client store. Persistence here is
  // fine: the user sets these explicitly and we want them back on
  // reload. When auto-scale is on, its live EMA overrides this path
  // and drives the renderer directly — see below.
  let manualFloorDbfs = $derived(clientControls.get('client.spectrum.displayFloorDbfs'));
  let manualCeilDbfs = $derived(clientControls.get('client.spectrum.displayCeilDbfs'));
  $effect(() => {
    if (!renderer || autoScale) return;
    renderer.setDisplayRange({ floorDbfs: manualFloorDbfs, ceilDbfs: manualCeilDbfs });
  });

  $effect(() => {
    renderer?.setFeatures({ fade, maxHold });
  });

  // Auto-scale: ephemeral, not persisted. Writes straight to the
  // renderer on every FFT frame — routing through `applyControl` +
  // `clientControls.set` used to localStorage-write ~30×/sec during
  // EMA convergence (`JSON.stringify` + `setItem` are synchronous and
  // block the main thread for ~1 ms each), visibly tanking FPS on
  // wideband presets. The auto-scale *toggle* still lives in the
  // store (`client.spectrum.autoScale`) — that's user intent — but
  // the computed floor/ceil are transient runtime state.
  const AUTO_SCALE_ALPHA = 0.08; // ~0.5s response at 30 Hz FFT
  const AUTO_FLOOR_MARGIN_DB = 5;
  const AUTO_CEIL_HEADROOM_DB = 10;
  const AUTO_MIN_WINDOW_DB = 20;

  let autoFloorEma: number | undefined;
  let autoCeilEma: number | undefined;

  $effect(() => {
    if (!renderer) return;
    if (!autoScale) {
      renderer.onStats(() => {});
      autoFloorEma = undefined;
      autoCeilEma = undefined;
      return;
    }
    renderer.onStats((stats) => {
      const f = SERVER_FLOOR_DBFS;
      const c = SERVER_CEIL_DBFS;
      const toDbfs = (byte: number) => f + (byte / 255) * (c - f);
      const p10Dbfs = toDbfs(stats.p10);
      const p99Dbfs = toDbfs(stats.p99);
      const floorTarget = p10Dbfs - AUTO_FLOOR_MARGIN_DB;
      const ceilTarget = p99Dbfs + AUTO_CEIL_HEADROOM_DB;
      autoFloorEma =
        autoFloorEma === undefined
          ? floorTarget
          : autoFloorEma * (1 - AUTO_SCALE_ALPHA) + floorTarget * AUTO_SCALE_ALPHA;
      autoCeilEma =
        autoCeilEma === undefined
          ? ceilTarget
          : autoCeilEma * (1 - AUTO_SCALE_ALPHA) + ceilTarget * AUTO_SCALE_ALPHA;
      let nextFloor = autoFloorEma;
      let nextCeil = autoCeilEma;
      if (nextCeil - nextFloor < AUTO_MIN_WINDOW_DB) {
        const mid = (nextFloor + nextCeil) / 2;
        nextFloor = mid - AUTO_MIN_WINDOW_DB / 2;
        nextCeil = mid + AUTO_MIN_WINDOW_DB / 2;
      }
      nextCeil = Math.min(nextCeil, c);
      renderer?.setDisplayRange({ floorDbfs: nextFloor, ceilDbfs: nextCeil });
    });
    return () => {
      renderer?.onStats(() => {});
    };
  });

  // Vertical markers over the plot: green = SDR tuner LO, orange = VFO
  // (only when the preset exposes a `freq_shift_hz` knob). They drop
  // into the renderer so they redraw with every frame, not just on
  // axes change.
  $effect(() => {
    if (!renderer) return;
    renderer.setMarkers({
      sdrCenterHz: axes?.center_freq_hz,
      vfoHz: vfoBlock ? vfoAbsHz : undefined,
      vfoWidthHz: vfoBlock ? vfoWidthHz : undefined,
      peaksHz: signals.signals.map((s) => s.freq_hz),
    });
  });

  // Hold a ref on the strongest-signal store whenever the preset
  // advertises a `ui:signals` sink, so the peak squares paint even when
  // the right-pane list view is closed. Refcounted in the store, so this
  // coexists with the list view's own attach.
  let signalsStreamId = $derived(pipeline.uiSinks.signals?.stream_id);
  $effect(() => {
    const c = pipeline.client;
    if (c && signalsStreamId !== undefined) {
      signals.attach(c, signalsStreamId);
      return () => signals.release();
    }
    return () => {};
  });

  // Frequency-allocation ribbon above the trace. Toggle is persisted in
  // the client control store; the data itself is a static build-time
  // import so flipping it is a pure render-side concern.
  $effect(() => {
    if (!renderer) return;
    renderer.setBandPlan(bandPlan ? bandplanUsa : undefined);
  });

  // Display-only zoom/pan into the FFT span. Renderer crops the trace,
  // grid and band ribbon to the view window; click-to-tune respects it.
  $effect(() => {
    renderer?.setView(viewWindow);
  });
</script>

<div class="flex h-full w-full flex-col">
  <div class="relative min-h-0 w-full flex-1">
    <canvas
      bind:this={canvas}
      tabindex="0"
      onwheel={onWheel}
      onkeydown={onKeydown}
      onpointerdown={(ev) => {
        canvas?.focus();
        ptr.onpointerdown(ev);
      }}
      onpointermove={ptr.onpointermove}
      onpointerup={ptr.onpointerup}
      onpointercancel={ptr.onpointercancel}
      onpointerleave={ptr.onpointerleave}
      class="block h-full w-full touch-none outline-none"
      class:cursor-grabbing={dragging}
      class:cursor-crosshair={!dragging}
      title="click: tune VFO · drag: fine-tune VFO · dbl-click: full tune · wheel: zoom · ↑↓: step · ←→: pan"
    ></canvas>
    {#if hoverPct !== null}
      <!-- Cross-pane VFO preview line. `pointer-events-none` so it
           doesn't intercept clicks meant for the canvas. -->
      <div
        class="pointer-events-none absolute top-0 bottom-0 w-px bg-sky-300/70"
        style:left="calc({LEFT_MARGIN}px + (100% - {LEFT_MARGIN + RIGHT_MARGIN}px) * {hoverPct}
        / 100)"
        style:box-shadow="0 0 3px rgba(125, 211, 252, 0.6)"
      ></div>
    {/if}
    <!-- Zoom overlay — anchored top-right inside the spectrum canvas,
         in the eye-line of the FFT it affects. Wheel-zoom-at-cursor is
         still the primary control; these buttons are the discoverable
         affordance. ⟲ only renders when zoom > 1. RIGHT_MARGIN-aware
         so the cluster sits inside the plot area, not over the axis
         label gutter. Each button is a fixed-size square with
         `inline-flex items-center justify-center` so the glyphs (−, +,
         ⟲ at different intrinsic heights) all paint at the same
         vertical centre line. -->
    <div
      class="pointer-events-none absolute top-1 flex items-center"
      style:right="{RIGHT_MARGIN + 6}px"
    >
      <div
        class="pointer-events-auto inline-flex h-6 items-center gap-1 rounded border border-slate-700/70 bg-slate-900/70 px-1 text-xs text-slate-200 backdrop-blur-sm"
      >
        {#if viewZoom > 1}
          <button
            type="button"
            class="inline-flex h-5 w-5 items-center justify-center rounded leading-none hover:bg-slate-700"
            onclick={resetZoom}
            title="reset zoom + pan"
            aria-label="reset zoom"
          >
            ⟲
          </button>
          <span class="font-mono text-[10px] leading-none text-slate-400"
            >{viewZoom.toFixed(1)}×</span
          >
        {/if}
        <button
          type="button"
          class="inline-flex h-5 w-5 items-center justify-center rounded leading-none hover:bg-slate-700 disabled:opacity-40"
          disabled={viewZoom <= 1}
          onclick={() => zoomBy(1 / ZOOM_STEP)}
          title="zoom out (wheel on FFT also works)"
          aria-label="zoom out">−</button
        >
        <button
          type="button"
          class="inline-flex h-5 w-5 items-center justify-center rounded leading-none hover:bg-slate-700 disabled:opacity-40"
          disabled={viewZoom >= MAX_ZOOM}
          onclick={() => zoomBy(ZOOM_STEP)}
          title="zoom in (wheel on FFT also works)"
          aria-label="zoom in">+</button
        >
      </div>
    </div>
  </div>
  {#if viewZoom > 1}
    <!-- Horizontal scrollbar for the zoomed FFT/waterfall view. Sized
         to match the plot's interior so the slider extents map directly
         onto the visible spectrum range. -->
    <div
      class="border-t border-slate-800 bg-[color:var(--color-bg)]"
      style:padding-left="44px"
      style:padding-right="6px"
    >
      <input
        type="range"
        class="block w-full"
        min={0}
        max={1}
        step={0.001}
        value={viewPan}
        title="pan zoomed view"
        oninput={(e) =>
          void applyControl(
            'client.spectrum.viewPan',
            Number((e.currentTarget as HTMLInputElement).value),
          )}
      />
    </div>
  {/if}
</div>
