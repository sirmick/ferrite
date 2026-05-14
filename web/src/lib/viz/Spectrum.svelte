<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { bandplanUsa } from '$lib/presets/bandplan';
  import { waterfallStore } from './waterfallStore.svelte';

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
    if (!axes) return undefined;
    const z = Math.max(1, viewZoom);
    if (z <= 1) return undefined;
    const span = axes.sample_rate_hz / z;
    const headroom = axes.sample_rate_hz - span;
    const fullMin = axes.center_freq_hz - axes.sample_rate_hz / 2;
    const viewMin = fullMin + Math.max(0, Math.min(1, viewPan)) * headroom;
    return { centerHz: viewMin + span / 2, rateHz: span };
  });

  // Committing the VFO writes the offset (`freq_shift_hz = target −
  // center`) so the absolute listening frequency matches what the user
  // typed. Clamp to the source span so the UI doesn't silently tune
  // outside what the channelizer can reach.
  function commitVfo(hz: number) {
    if (!axes || !vfoBlock) return;
    const shift = hz - axes.center_freq_hz;
    const half = axes.sample_rate_hz / 2;
    const clamped = Math.max(-half, Math.min(half, shift));
    if (clamped !== vfoShiftHz) {
      void applyControl(`flow.${vfoBlock.id}.freq_shift_hz`, clamped);
    }
  }

  function onWheel(ev: WheelEvent) {
    // Wheel down/right → pan view right (increase viewPan).
    // No-op when zoom is 1× because there's no headroom to pan into.
    if (viewZoom <= 1) return;
    ev.preventDefault();
    const dy = ev.deltaY;
    const dx = ev.deltaX;
    const delta = Math.abs(dx) > Math.abs(dy) ? dx : dy;
    if (delta === 0) return;
    const sign = delta > 0 ? 1 : -1;
    const next = Math.max(0, Math.min(1, viewPan + sign * WHEEL_PAN_FRACTION));
    if (next !== viewPan) void applyControl('client.spectrum.viewPan', next);
  }

  // Click = VFO; double-click = SDR centre. Native `click` fires twice
  // before `dblclick`, so we hold the single-click action in a timer
  // and cancel it if the second click lands within the OS double-click
  // window.
  const DBLCLICK_MS = 250;
  let pendingSingle: ReturnType<typeof setTimeout> | undefined;

  function freqAtPointer(ev: MouseEvent): number | undefined {
    if (!renderer || !canvas) return undefined;
    const rect = canvas.getBoundingClientRect();
    return renderer.pixelToFreq(ev.clientX - rect.left);
  }

  function onClick(ev: MouseEvent) {
    const f = freqAtPointer(ev);
    if (f === undefined) return;
    // Defer so a following dblclick can cancel. If there's no VFO
    // block there's nothing for the single-click to do that dblclick
    // wouldn't also do — short-circuit and centre immediately.
    if (!vfoBlock || !axes) {
      void applyControl('flow.src.center_freq_hz', Math.round(f));
      return;
    }
    if (pendingSingle !== undefined) clearTimeout(pendingSingle);
    pendingSingle = setTimeout(() => {
      pendingSingle = undefined;
      commitVfo(Math.round(f));
    }, DBLCLICK_MS);
  }

  function onDblClick(ev: MouseEvent) {
    if (pendingSingle !== undefined) {
      clearTimeout(pendingSingle);
      pendingSingle = undefined;
    }
    const f = freqAtPointer(ev);
    if (f === undefined) return;
    // Centre freq is on the live-apply whitelist — hot retune, no rebuild.
    void applyControl('flow.src.center_freq_hz', Math.round(f));
  }

  onMount(() => {
    if (!canvas) return;
    renderer = new SpectrumRenderer(canvas);
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    // The "reset" button lives in DisplayControls (sibling of the
    // waterfall pane); install a renderer-bound impl on the shared
    // store so a click reaches our renderer here. Cleared on teardown.
    waterfallStore.resetMaxHold = () => renderer?.resetMaxHold();
    return () => {
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
    });
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
  <canvas
    bind:this={canvas}
    onclick={onClick}
    ondblclick={onDblClick}
    onwheel={onWheel}
    class="block min-h-0 w-full flex-1 cursor-crosshair"
    title="click to tune VFO · double-click to re-centre SDR · wheel to pan zoomed view"
  ></canvas>
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
