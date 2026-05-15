<script lang="ts">
  // Curated "quick" source controls — gain, antenna, AGC, DC tracking.
  // Mounted next to the nixies in the spectrum header. Every commit goes
  // through `pipeline.patchSourceParams`; the server decides whether the
  // delta is live-applicable (whitelist: `gain_db`, `antenna`, `agc`,
  // `dc_offset_correction`, `center_freq_hz`) or needs a rebuild. Same
  // wire path as the advanced panel, just a curated subset of controls.

  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import {
    gainDescriptionFor,
    gainDisplayValue,
    gainInvertedFor,
    gainLabelFor,
    gainRawValue,
  } from './optionsModel';

  let caps = $derived(
    pipeline.sourceCaps?.kind === 'hardware' ? pipeline.sourceCaps.capabilities : null,
  );
  let channel = $derived(caps?.rx_channels[0] ?? null);
  let params = $derived((pipeline.source?.params ?? {}) as Record<string, unknown>);

  let overallRange = $derived(channel?.overall_gain_range_db ?? null);
  let antennas = $derived(channel?.antennas ?? []);
  let hasAgc = $derived(channel?.has_agc ?? false);
  let hasDcOffset = $derived(channel?.has_dc_offset_mode ?? false);
  // Driver-specific master-gain label/tooltip — see optionsModel.ts.
  // The toolbar label is short ("IF gain" minus the unit suffix) so it
  // doesn't crowd the row; the tooltip carries the full description.
  let gainLabel = $derived(caps ? gainLabelFor(caps).replace(/\s*\(dB\)\s*$/, '') : 'gain');
  let gainTooltip = $derived(
    caps ? (gainDescriptionFor(caps) ?? 'receiver gain (dB)') : 'receiver gain (dB)',
  );

  let gainDb = $derived(numberOr(params.gain_db, overallRange?.min ?? 0));
  let antenna = $derived(typeof params.antenna === 'string' ? (params.antenna as string) : '');
  let agc = $derived(params.agc === true);
  // Default-on: only an explicit `false` disables the driver tracker.
  let dcOffset = $derived(params.dc_offset_correction !== false);

  // Master-slider semantics — see optionsModel.ts. SDRplay-style
  // drivers report gain reduction; we invert the displayed value so
  // the slider's "higher = more amplification" intuition holds.
  let gainInverted = $derived(caps ? gainInvertedFor(caps) : false);
  let gainDisplay = $derived(
    overallRange ? gainDisplayValue(gainDb, overallRange, gainInverted) : gainDb,
  );

  let visible = $derived(
    !!channel && (overallRange !== null || antennas.length > 1 || hasAgc || hasDcOffset),
  );

  function numberOr(v: unknown, fallback: number): number {
    const n = typeof v === 'number' ? v : Number(v);
    return Number.isFinite(n) ? n : fallback;
  }

  // Every commit goes through `patchSourceParams`. The server inspects
  // the delta: whitelisted keys live-apply (no rebuild); others trigger
  // full source reconstruction. Kept deliberately simple here — no
  // client-side branching on which keys are hot. Same wire path the
  // advanced panel uses.

  // Trailing-edge debounce for the scrubbable gain slider. `oninput`
  // fires on every pixel of the drag; without this we'd ship ~60
  // patches/sec for a one-second drag, overwhelming the hot path even
  // though Soapy itself would accept them all. 50ms smooths the drag to
  // ~20 patches/sec — imperceptible lag, vastly less churn.
  const GAIN_DEBOUNCE_MS = 50;
  let gainDebounce: ReturnType<typeof setTimeout> | undefined;
  let pendingGain: number | undefined;

  /// Slider's onInput sends the **displayed** value; we map it back to
  /// the raw `gain_db` here before dispatching. With `gain_inverted=
  /// false` this is identity, so non-SDRplay drivers see no behaviour
  /// change.
  function commitGainDisplay(displayed: number) {
    const range = overallRange;
    if (!range) return;
    const raw = gainRawValue(displayed, range, gainInverted);
    if (raw === gainDb) return;
    pendingGain = raw;
    if (gainDebounce !== undefined) clearTimeout(gainDebounce);
    gainDebounce = setTimeout(() => {
      gainDebounce = undefined;
      const next = pendingGain;
      pendingGain = undefined;
      if (next !== undefined && next !== gainDb) {
        void applyControl('flow.src.gain_db', next);
      }
    }, GAIN_DEBOUNCE_MS);
  }
  function commitAntenna(v: string) {
    if (v === antenna) return;
    void applyControl('flow.src.antenna', v);
  }
  function commitAgc(v: boolean) {
    if (v === agc) return;
    void applyControl('flow.src.agc', v);
  }
  function commitDcOffset(v: boolean) {
    if (v === dcOffset) return;
    void applyControl('flow.src.dc_offset_correction', v);
  }
</script>

{#if visible}
  <div class="mx-1 h-4 border-l border-slate-800"></div>

  {#if overallRange}
    <label class="flex items-center gap-1" title={gainTooltip}>
      <span>{gainLabel}</span>
      <input
        type="range"
        class="w-24"
        min={overallRange.min}
        max={overallRange.max}
        step={overallRange.step ?? 1}
        value={gainDisplay}
        oninput={(e) => commitGainDisplay(Number((e.currentTarget as HTMLInputElement).value))}
      />
      <span class="w-10 text-right font-mono text-slate-300">{gainDisplay.toFixed(1)}</span>
      <span>dB</span>
    </label>
  {/if}

  {#if antennas.length > 1}
    <label class="flex items-center gap-1" title="RF antenna port">
      <span>ant</span>
      <select
        class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
        value={antenna}
        onchange={(e) => commitAntenna((e.currentTarget as HTMLSelectElement).value)}
      >
        {#each antennas as a (a)}
          <option value={a}>{a}</option>
        {/each}
      </select>
    </label>
  {/if}

  {#if hasAgc}
    <label class="flex items-center gap-1" title="automatic gain control">
      <input
        type="checkbox"
        checked={agc}
        onchange={(e) => commitAgc((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>agc</span>
    </label>
  {/if}

  {#if hasDcOffset}
    <label class="flex items-center gap-1" title="driver DC-offset (LO-leakage) tracking">
      <input
        type="checkbox"
        checked={dcOffset}
        onchange={(e) => commitDcOffset((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>dc</span>
    </label>
  {/if}
{/if}
