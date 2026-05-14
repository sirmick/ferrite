<script lang="ts">
  // RF quick-control bar — the always-visible "convenience" row that
  // sits directly under the AppToolbar. Carries:
  //
  //  - VFO nixie + SDR-centre nixie + sample-rate dropdown + LiveControls
  //    (gain / antenna / per-driver toggle settings) — left cluster,
  //    gated on live axes (only meaningful once a pipeline is running).
  //  - Channel toggle (show/hide the channelised FFT/waterfall pane) —
  //    right-aligned, always visible. Disabled on presets without a
  //    Channelizer (no `ui:fft_narrow` sink).
  //
  // Action buttons (Start/Stop, Source…, Flowgraph…) live one row up
  // in AppToolbar so they're always reachable, even with no source
  // configured. The outer container here renders unconditionally so
  // the Channel toggle stays in its same spot as Start, status chips,
  // and the visual knobs (DisplayControls).
  //
  // Reads pipeline state directly; no props.
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { quickRateChoices, bandwidthForRate } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';
  import LiveControls from '$lib/controls/LiveControls.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { applyControl } from '$lib/control/dispatch';

  let axes = $derived(currentAxes(pipeline));

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

  // Channel-detail column toggle. Lives here (next to the other
  // navigational knobs) rather than in DisplayControls — the
  // visual-shape strip — because show/hide of an entire spectrum pane
  // is a navigation move, not a visual tweak.
  let hasNarrow = $derived(pipeline.uiSinks.fft_narrow !== undefined);
  let narrowVisible = $derived(clientControls.get('client.workspace.narrowVisible'));

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

  function commitVfo(hz: number) {
    if (!axes || !vfoBlock) return;
    const shift = hz - axes.center_freq_hz;
    const half = axes.sample_rate_hz / 2;
    const clamped = Math.max(-half, Math.min(half, shift));
    if (clamped !== vfoShiftHz) {
      void applyControl(`flow.${vfoBlock.id}.freq_shift_hz`, clamped);
    }
  }
</script>

<!-- Always renders so the Channel toggle stays anchored to a stable
     spot. VFO + centre + rate + LiveControls light up once the
     pipeline has live axes. -->
<div
  class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
>
  {#if axes}
    {#if vfoBlock}
      <span class="contents" title="VFO — what you're listening to (orange)">
        <Nixie hz={vfoAbsHz} onCommit={commitVfo} tone="orange" />
      </span>
    {/if}

    <span class="contents" title="SDR centre — the RF tuner LO (green)">
      <Nixie hz={axes.center_freq_hz} onCommit={commitCenter} tone="green" />
    </span>

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
  {/if}

  <!-- Channel-detail toggle — right-aligned, always visible. Disabled
       when the active preset has no Channelizer (no `ui:fft_narrow`
       sink injected by env_split). -->
  <button
    type="button"
    class="ml-auto rounded border border-slate-700 px-2 py-0.5 text-[11px] hover:border-slate-600 disabled:cursor-not-allowed disabled:opacity-40"
    class:channel-on={hasNarrow && narrowVisible}
    disabled={!hasNarrow}
    title={hasNarrow
      ? narrowVisible
        ? 'Hide channel-detail FFT / waterfall'
        : 'Show channel-detail FFT / waterfall'
      : 'Active preset has no channelizer — no channel-detail pane available'}
    onclick={() => void applyControl('client.workspace.narrowVisible', !narrowVisible)}
  >
    Channel
  </button>
</div>

<style>
  /* Sky-blue accent matches the VFO marker on the wide spectrum /
     waterfall — same hue means "you're looking at the channelised
     slice" everywhere. */
  .channel-on {
    border-color: rgb(125 211 252);
    color: rgb(125 211 252);
  }
  .channel-on:hover {
    border-color: rgb(186 230 253);
  }
</style>
