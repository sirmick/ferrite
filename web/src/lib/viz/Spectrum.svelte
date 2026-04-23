<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { quickRateChoices, bandwidthForRate } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';
  import LiveControls from '$lib/controls/LiveControls.svelte';
  import FftControls from './FftControls.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';

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

  // Fixed wheel-nudge step. The nixie handles fine tuning (each digit
  // is click-to-increment), so this just needs to be a sensible scroll
  // rate for exploring the band.
  const WHEEL_STEP_HZ = 10_000;

  // Display toggles live in the client control store so they persist
  // across reloads and stay read/write through the same `applyControl`
  // dispatch path every other knob uses.
  let fade = $derived(clientControls.get('client.spectrum.fade'));
  let maxHold = $derived(clientControls.get('client.spectrum.maxHold'));
  let autoScale = $derived(clientControls.get('client.spectrum.autoScale'));

  // Sample-rate dropdown by the nixies: preset-curated short list.
  // Software sources (sine, file) have no advertised rate ladder, so
  // the list is empty and the UI collapses to a read-only display.
  // The current rate is always included even when it's not in the
  // curated list — the user may have set it explicitly from the
  // advanced panel in `InputControls`, and leaving the value absent
  // here would render the `<select>` blank.
  let rateChoices = $derived.by(() => {
    const caps = pipeline.sourceCaps;
    if (!caps || caps.kind !== 'hardware') return [] as number[];
    const choices = quickRateChoices(caps.capabilities);
    const rate = axes?.sample_rate_hz;
    if (rate !== undefined && !choices.some((c) => Math.abs(c - rate) < 1)) {
      choices.push(rate);
      choices.sort((a, b) => a - b);
    }
    return choices;
  });

  function fmtRate(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MS/s`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kS/s`;
    return `${hz} S/s`;
  }

  function onRateChange(ev: Event) {
    const v = Number((ev.target as HTMLSelectElement).value);
    if (!Number.isFinite(v) || v === axes?.sample_rate_hz) return;
    // Atomic two-key write: Fs + BW must land in one patch so the
    // server sees both in the same reconfigure pass (driver-specific
    // IF filter ladder on SDRplay etc.). `patchSourceParams` is the
    // escape hatch from `applyControl` for multi-key flow writes.
    const caps = pipeline.sourceCaps;
    const patch: Record<string, unknown> = { sample_rate_hz: v };
    if (caps?.kind === 'hardware') {
      const bw = bandwidthForRate(caps.capabilities, v);
      if (bw !== null) patch.bandwidth_hz = bw;
    }
    void pipeline.patchSourceParams(patch);
  }

  function commitCenter(hz: number) {
    if (axes && hz !== axes.center_freq_hz) {
      void applyControl('flow.src.center_freq_hz', hz);
    }
  }

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
    if (!axes) return;
    ev.preventDefault();
    const sign = ev.deltaY > 0 ? -1 : 1;
    void applyControl('flow.src.center_freq_hz', axes.center_freq_hz + sign * WHEEL_STEP_HZ);
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
    return () => {
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

  // The display range that actually drives the y-axis labels and the
  // byte→pixel LUT. Auto-scale writes here; manual overrides (not yet
  // UI-exposed as inputs) can be added later. Kept independent from
  // the fixed server window.
  let displayFloorDbfs = $derived(clientControls.get('client.spectrum.displayFloorDbfs'));
  let displayCeilDbfs = $derived(clientControls.get('client.spectrum.displayCeilDbfs'));
  $effect(() => {
    if (!renderer) return;
    renderer.setDisplayRange({ floorDbfs: displayFloorDbfs, ceilDbfs: displayCeilDbfs });
  });

  $effect(() => {
    renderer?.setFeatures({ fade, maxHold });
  });

  // Auto-scale: pure client-side display stretch. Chases the signal's
  // running p10/p99 and writes back into the client store's
  // `displayFloorDbfs`/`displayCeilDbfs` so the renderer sees it via
  // the $derived reads above. Persists across reloads for free.
  const AUTO_SCALE_ALPHA = 0.08; // ~0.5s response at 30 Hz FFT
  const AUTO_FLOOR_MARGIN_DB = 5;
  const AUTO_CEIL_HEADROOM_DB = 10;
  const AUTO_MIN_WINDOW_DB = 20;
  const AUTO_COMMIT_HYSTERESIS_DB = 0.5; // store writes only on meaningful moves

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
      // Skip writes that would change the stored value by less than
      // the meter's noise floor — `applyControl` persists to localStorage
      // on every set, and driving it per-frame is wasteful.
      const curF = clientControls.get('client.spectrum.displayFloorDbfs');
      const curC = clientControls.get('client.spectrum.displayCeilDbfs');
      if (Math.abs(nextFloor - curF) >= AUTO_COMMIT_HYSTERESIS_DB) {
        void applyControl('client.spectrum.displayFloorDbfs', Math.round(nextFloor * 10) / 10);
      }
      if (Math.abs(nextCeil - curC) >= AUTO_COMMIT_HYSTERESIS_DB) {
        void applyControl('client.spectrum.displayCeilDbfs', Math.round(nextCeil * 10) / 10);
      }
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
</script>

<div class="flex h-full w-full flex-col">
  {#if axes}
    <div
      class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
    >
      {#if vfoBlock}
        <div class="flex items-center gap-1" title="VFO — what you're listening to">
          <span class="mr-0.5 text-[9px] uppercase tracking-wider text-orange-400/70">vfo</span>
          <Nixie hz={vfoAbsHz} onCommit={commitVfo} tone="orange" />
        </div>
      {/if}

      <div class="flex items-center gap-1" title="SDR centre — the RF tuner LO">
        <span class="mr-0.5 text-[9px] uppercase tracking-wider text-emerald-400/70">sdr</span>
        <Nixie hz={axes.center_freq_hz} onCommit={commitCenter} tone="green" />
      </div>

      <label class="flex items-center gap-1" title="sample rate / span">
        <span>rate</span>
        {#if rateChoices.length > 1}
          <select
            class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
            value={axes.sample_rate_hz}
            onchange={onRateChange}
          >
            {#each rateChoices as r (r)}
              <option value={r}>{fmtRate(r)}</option>
            {/each}
          </select>
        {:else}
          <span class="font-mono text-slate-300">{fmtRate(axes.sample_rate_hz)}</span>
        {/if}
      </label>

      <LiveControls />

      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1" title="fade trail of recent traces">
        <input
          type="checkbox"
          checked={fade}
          onchange={(e) =>
            void applyControl(
              'client.spectrum.fade',
              (e.currentTarget as HTMLInputElement).checked,
            )}
        />
        <span>fade</span>
      </label>
      <label class="flex items-center gap-1" title="running max-hold trace">
        <input
          type="checkbox"
          checked={maxHold}
          onchange={(e) =>
            void applyControl(
              'client.spectrum.maxHold',
              (e.currentTarget as HTMLInputElement).checked,
            )}
        />
        <span>max hold</span>
      </label>
      {#if maxHold}
        <button
          type="button"
          class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
          onclick={() => renderer?.resetMaxHold()}
        >
          reset
        </button>
      {/if}
      <label
        class="flex items-center gap-1"
        title="auto-track floor/ceil to the signal (writes logmag.floor_dbfs/ceil_dbfs)"
      >
        <input
          type="checkbox"
          checked={autoScale}
          onchange={(e) =>
            void applyControl(
              'client.spectrum.autoScale',
              (e.currentTarget as HTMLInputElement).checked,
            )}
        />
        <span>auto</span>
      </label>
    </div>
    <FftControls />
  {/if}
  <canvas
    bind:this={canvas}
    onclick={onClick}
    ondblclick={onDblClick}
    onwheel={onWheel}
    class="block min-h-0 w-full flex-1 cursor-crosshair"
    title="click to tune VFO · double-click to re-centre SDR · wheel to nudge"
  ></canvas>
</div>
