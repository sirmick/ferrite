<script lang="ts">
  // RF quick-control bar — the VFO + SDR-centre nixies, the curated
  // sample-rate dropdown, and the LiveControls slot (gain / antenna /
  // toggle settings). Lifted out of `Spectrum.svelte`'s internal header
  // row so it lives at the top of the wide workspace column rather
  // than nested inside the spectrum pane. Click-to-tune still happens
  // on the spectrum canvas — this bar handles type-in tuning and
  // discrete-step rate selection.
  //
  // Reads pipeline state directly; no props. The same shape used to live
  // inline in Spectrum and pulled the same deriveds.
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { quickRateChoices, bandwidthForRate } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';
  import LiveControls from '$lib/controls/LiveControls.svelte';
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

{#if axes}
  <div
    class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
  >
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
  </div>
{/if}
