<script lang="ts">
  // Companion to `Waterfall.svelte` that subscribes to the runtime-
  // injected `ui:fft_narrow` sink (see `inject_narrow_fft.rs`). The
  // narrow waterfall renders the channelizer's output — a single
  // channel post-VFO-shift — at much higher per-bin resolution than
  // the wide source-side waterfall.
  //
  // Differences from `Waterfall.svelte`:
  //
  //   - No zoom/pan. The narrow view IS already zoomed; further
  //     window math doesn't apply.
  //   - Click + drag fine-tune the VFO inside the channel window — same
  //     `createPointerTune` factory the other panes use, so behaviour is
  //     uniform.
  //   - No VFO marker (the VFO IS the centre).
  //   - Axes derive from the channelizer's `freq_shift_hz` +
  //     `output_rate_hz`, not from the source.
  //   - Renders nothing (hidden) when no `ui:fft_narrow` sink is in
  //     `pipeline.uiSinks` — i.e. on bareband presets without a
  //     Channelizer, which is the only case `inject_narrow_fft` skips.
  import { onMount } from 'svelte';
  import { LEFT_MARGIN, RIGHT_MARGIN } from './spectrum';
  import { WaterfallRenderer } from './waterfall';
  import { waterfallStore } from './waterfallStore.svelte';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { dragVfoExact, tuneVfoExact } from '$lib/control/tuning.svelte';
  import { createPointerTune } from '$lib/control/pointerTune';
  import { hoverStore, hoverPctInWindow } from './hoverStore.svelte';
  import { registerView, unregisterView, dataUrlToBase64 } from './viewRegistry';

  interface Props {
    client: FrameClient;
    rows?: number;
  }

  let { client, rows = 320 }: Props = $props();

  // The narrow-FFT sink name matches the runtime convention in
  // `inject_narrow_fft.rs`. Single-channelizer presets get this exact
  // key; multi-channelizer presets get `fft_narrow_<chan_id>` — TODO
  // when that day comes, expose a list. For now the common case is
  // single.
  let fftStreamId = $derived(pipeline.uiSinks.fft_narrow?.stream_id);

  // Channelizer-derived axes. Look up by `freq_shift_hz` param
  // presence, same convention `Spectrum.svelte` and `Waterfall.svelte`
  // use to find the VFO block. The narrow waterfall's centre frequency
  // is the source centre + the channelizer's VFO shift (i.e. the
  // channel the operator is listening to in absolute Hz). Its rate is
  // the channelizer's `output_rate_hz`.
  let wideAxes = $derived(currentAxes(pipeline));
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
  let narrowCenterHz = $derived(wideAxes ? wideAxes.center_freq_hz + vfoShiftHz : undefined);
  let narrowRateHz = $derived.by(() => {
    if (!vfoValues || !wideAxes) return undefined;
    const outRate = vfoValues.output_rate_hz;
    if (typeof outRate === 'number' && outRate > 0) return outRate;
    const factor = vfoValues.factor;
    if (typeof factor === 'number' && factor > 0) return wideAxes.sample_rate_hz / factor;
    return undefined;
  });

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: WaterfallRenderer | undefined;

  // Pointer-to-tune: same contract as the other panes — click → full
  // VFO tune, drag → live channelizer freq_shift retune with the freq
  // axis frozen at pointer-down so the view's VFO-following recentre
  // doesn't chase the cursor.
  let dragging = $state(false);
  const ptr = createPointerTune({
    getCanvas: () => canvas,
    getAxis: () =>
      narrowCenterHz !== undefined && narrowRateHz !== undefined && narrowRateHz > 0
        ? { centerHz: narrowCenterHz, rateHz: narrowRateHz }
        : undefined,
    onClick: (hz) => {
      void dragVfoExact(hz);
    },
    onDoubleClick: (hz) => tuneVfoExact(hz),
    onDrag: ({ targetHz }) => dragVfoExact(targetHz),
    onDragChange: (d) => (dragging = d),
    onHover: (hz) => (hoverStore.freqHz = hz),
  });

  // Cross-pane hover preview — `null` when the hovered freq is outside
  // this channel window.
  let hoverPct = $derived(hoverPctInWindow(hoverStore.freqHz, narrowCenterHz, narrowRateHz));

  // Reuse the same contrast / auto-contrast state the wide waterfall
  // uses. Same dBFS-to-byte mapping (LogMagU8 quantises both wide and
  // narrow streams to the same [-160, 0] dBFS window), so identical
  // bytes paint identical colours — required for the two panes to
  // look comparable when the operator drags the manual range slider.
  const WF_FLOOR_DBFS = -160;
  const WF_CEIL_DBFS = 0;
  function dbfsToByte01(d: number): number {
    return (d - WF_FLOOR_DBFS) / (WF_CEIL_DBFS - WF_FLOOR_DBFS);
  }
  let autoContrast = $derived(clientControls.get('client.waterfall.autoContrast'));
  let floorDbfs = $derived(clientControls.get('client.waterfall.contrastFloorDbfs'));
  let ceilDbfs = $derived(clientControls.get('client.waterfall.contrastCeilDbfs'));

  onMount(() => {
    if (!canvas) return;
    renderer = new WaterfallRenderer(canvas, { rows });
    renderer.setAutoContrast(autoContrast);
    renderer.setManualContrast(dbfsToByte01(floorDbfs), dbfsToByte01(ceilDbfs));
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    // Register for `ferrite-ctl view channel-waterfall`.
    const snapshot = () => dataUrlToBase64(canvas!.toDataURL('image/png'));
    registerView('channel-waterfall', snapshot);
    return () => {
      unregisterView('channel-waterfall', snapshot);
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  $effect(() => {
    if (!renderer) return;
    renderer.setAutoContrast(autoContrast);
    renderer.setManualContrast(dbfsToByte01(floorDbfs), dbfsToByte01(ceilDbfs));
  });

  // Mirror the wide waterfall's auto-contrast window. The wide
  // renderer publishes its computed (smoothed) bounds into
  // `waterfallStore.sharedAutoFloor01/Ceil01`; when those are set
  // and auto-contrast is on, we drive our window from them instead
  // of running our own percentile detector. Same byte window across
  // both panes → same palette colours for the same dB signal.
  $effect(() => {
    if (!renderer) return;
    const floor = waterfallStore.sharedAutoFloor01;
    const ceil = waterfallStore.sharedAutoCeil01;
    renderer.setExternalAutoBounds(floor, ceil);
  });

  $effect(() => {
    const sid = fftStreamId;
    if (sid === undefined) return;
    const unsub = client.subscribe(sid, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      if (waterfallStore.paused) return;
      renderer?.pushRow(frame.payload);
    });
    return unsub;
  });
</script>

{#if fftStreamId !== undefined}
  <div class="relative flex h-full w-full flex-col">
    <div
      class="relative min-h-0 flex-1"
      style:padding-left="{LEFT_MARGIN}px"
      style:padding-right="{RIGHT_MARGIN}px"
    >
      <canvas
        bind:this={canvas}
        onpointerdown={ptr.onpointerdown}
        onpointermove={ptr.onpointermove}
        onpointerup={ptr.onpointerup}
        onpointercancel={ptr.onpointercancel}
        onpointerleave={ptr.onpointerleave}
        class="block h-full w-full touch-none"
        class:cursor-grabbing={dragging}
        class:cursor-crosshair={!dragging}
        title="click: tune VFO · drag: fine-tune VFO · dbl-click: full tune"
      ></canvas>
      {#if hoverPct !== null}
        <!-- Cross-pane VFO preview line. -->
        <div
          class="pointer-events-none absolute top-0 bottom-0 w-px bg-sky-300/70"
          style:left="calc({LEFT_MARGIN}px + (100% - {LEFT_MARGIN + RIGHT_MARGIN}px) * {hoverPct}
          / 100)"
          style:box-shadow="0 0 3px rgba(125, 211, 252, 0.6)"
        ></div>
      {/if}
    </div>
    <!-- Axis label band underneath. Min/centre/max in absolute Hz so
         the operator can read off the channel they're listening to
         without doing mental arithmetic from the wide view. -->
    {#if narrowCenterHz !== undefined && narrowRateHz !== undefined && narrowRateHz > 0}
      {@const minHz = narrowCenterHz - narrowRateHz / 2}
      {@const maxHz = narrowCenterHz + narrowRateHz / 2}
      {@const fmt = (hz: number) => `${(hz / 1e6).toFixed(3)} MHz`}
      <div
        class="flex items-center justify-between border-t border-slate-800 px-2 py-0.5 font-mono text-[10px] text-slate-400"
        style:padding-left="{LEFT_MARGIN + 4}px"
        style:padding-right="{RIGHT_MARGIN + 4}px"
      >
        <span>{fmt(minHz)}</span>
        <span class="text-slate-300">VFO {fmt(narrowCenterHz)}</span>
        <span>{fmt(maxHz)}</span>
      </div>
    {/if}
  </div>
{/if}
