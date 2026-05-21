<script lang="ts">
  // RF quick-control bar — the always-visible "convenience" row that
  // sits directly under the AppToolbar. Carries:
  //
  //  - VFO nixie + step/snap control + SDR-centre nixie + sample-rate
  //    dropdown + LiveControls (gain / antenna / per-driver settings)
  //    — left cluster, gated on live axes.
  //  - Display-zoom control — right-aligned.
  //
  // Action buttons (Start/Stop, Source…, Flowgraph…, Channel) live one
  // row up in AppToolbar so they're always reachable. Reads pipeline
  // state directly; no props.
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { quickRateChoices, bandwidthForRate } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';
  import LiveControls from '$lib/controls/LiveControls.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { tuneVfoTo, stepVfo, currentTuning } from '$lib/control/tuning.svelte';

  let axes = $derived(currentAxes(pipeline));

  // Tuning step / snap (band-plan aware via tuningModel). The dropdown
  // is the single control: 'off' = free tune (no snap), 'auto' =
  // band-derived step + snap, a fixed Hz = that raster + snap.
  let stepMode = $derived(clientControls.get('client.tuning.stepMode'));
  let tuning = $derived(currentTuning());
  const STEP_CHOICES: { v: string; label: string }[] = [
    { v: 'off', label: 'Off' },
    { v: 'auto', label: 'Auto' },
    { v: '100', label: '100 Hz' },
    { v: '1000', label: '1 kHz' },
    { v: '5000', label: '5 kHz' },
    { v: '9000', label: '9 kHz' },
    { v: '10000', label: '10 kHz' },
    { v: '12500', label: '12.5 kHz' },
    { v: '25000', label: '25 kHz' },
    { v: '100000', label: '100 kHz' },
    { v: '200000', label: '200 kHz' },
  ];
  function fmtStep(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(hz % 1e6 ? 3 : 0)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(hz % 1e3 ? 1 : 0)} kHz`;
    return `${hz} Hz`;
  }

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
    // IF filter ladder on SDRplay / HackRF etc.).
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

  // VFO commit routes through the central snap-aware path.
  function commitVfo(hz: number) {
    tuneVfoTo(hz);
  }
</script>

<div
  class="flex flex-wrap items-center gap-x-2 gap-y-0.5 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-0.5 text-[11px] text-[color:var(--color-muted)]"
>
  {#if axes}
    {#if vfoBlock}
      <span class="contents" title="VFO — what you're listening to (orange)">
        <Nixie hz={vfoAbsHz} onCommit={commitVfo} tone="orange" />
      </span>

      <span
        class="flex items-center gap-1"
        title="Tuning step (Up/Down keys or ▲▼). Off = free tune, no snap."
      >
        <button
          type="button"
          class="rounded border border-slate-700 px-1.5 py-1 leading-none hover:border-slate-600"
          aria-label="step down"
          onclick={() => stepVfo(-1)}>▼</button
        >
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1.5 py-1 text-slate-200"
          value={stepMode}
          onchange={(e) =>
            void applyControl(
              'client.tuning.stepMode',
              (e.currentTarget as HTMLSelectElement).value,
            )}
        >
          {#each STEP_CHOICES as c (c.v)}
            <option value={c.v}>
              {c.v === 'auto' ? `Auto (${fmtStep(tuning.stepHz)})` : c.label}
            </option>
          {/each}
        </select>
        <button
          type="button"
          class="rounded border border-slate-700 px-1.5 py-1 leading-none hover:border-slate-600"
          aria-label="step up"
          onclick={() => stepVfo(1)}>▲</button
        >
      </span>
    {/if}

    <span class="contents" title="SDR centre — the RF tuner LO (green)">
      <Nixie hz={axes.center_freq_hz} onCommit={commitCenter} tone="green" />
    </span>

    <!-- Sample rate / span. Unit suffix in the values (MS/s / kS/s) so
         the leading "rate" label was redundant — dropped to claw back
         horizontal space. -->
    <label class="flex items-center" title="sample rate / span">
      {#if rateChoices.length > 1}
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1.5 py-1 text-slate-200"
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

    <!-- Display-zoom lived here until 2026-05-21 — a horizontal slider
         claiming the right edge of the bar. The spectrum already has
         wheel-zoom-at-cursor (the primary input), and dedicated +/−/⟲
         buttons now sit inside the spectrum canvas's top-right corner,
         in the eye-line of the FFT they affect. Frees the bar for
         RF / source state. -->
  {/if}
</div>
