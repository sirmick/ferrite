<script lang="ts">
  import { Dialog } from 'bits-ui';
  import DevicePicker from './DevicePicker.svelte';
  import SamplePicker from './SamplePicker.svelte';
  import PresetJsonView from './PresetJsonView.svelte';
  import type { SourceConfig } from '$lib/api/source';
  import type { DeviceCapabilities } from '$lib/api/devices';
  import { untrack } from 'svelte';

  interface Props {
    open: boolean;
    /** Current source config — seeds the Tone form when its type is sine. */
    source: SourceConfig | null;
    /** Selected a Soapy device: parent applies a default SourceConfig. */
    onPickDevice: (caps: DeviceCapabilities) => void;
    /** Applied a source-config from any sub-form — parent PATCHes it. */
    onApply: (cfg: SourceConfig) => void;
    onClose: () => void;
  }

  let { open = $bindable(), source, onPickDevice, onApply, onClose }: Props = $props();

  const CENTER_FREQ_HZ = 100_000_000;
  const DEFAULT_SINE_RATE_HZ = 2_000_000;

  function sineParamsFrom(cfg: SourceConfig | null) {
    const p = cfg?.type === 'SineSource' ? cfg.params : {};
    return {
      rate_hz: (p.rate_hz as number | undefined) ?? DEFAULT_SINE_RATE_HZ,
      center_freq_hz: (p.center_freq_hz as number | undefined) ?? CENTER_FREQ_HZ,
      tone_freq_abs_hz: (p.tone_freq_abs_hz as number | undefined) ?? CENTER_FREQ_HZ + 1_000,
      amplitude: (p.amplitude as number | undefined) ?? 0.25,
    };
  }

  let seed = untrack(() => sineParamsFrom(source));
  let toneOffsetKHz = $state(Math.round((seed.tone_freq_abs_hz - seed.center_freq_hz) / 1000));
  let amplitude = $state(seed.amplitude);
  let centerFreqMHz = $state(seed.center_freq_hz / 1e6);
  let rateMHz = $state(seed.rate_hz / 1e6);

  $effect(() => {
    if (open) {
      const s = sineParamsFrom(source);
      toneOffsetKHz = Math.round((s.tone_freq_abs_hz - s.center_freq_hz) / 1000);
      amplitude = s.amplitude;
      centerFreqMHz = s.center_freq_hz / 1e6;
      rateMHz = s.rate_hz / 1e6;
    }
  });

  function applyTone() {
    const center = Math.round(centerFreqMHz * 1e6);
    onApply({
      type: 'SineSource',
      params: {
        rate_hz: Math.round(rateMHz * 1e6),
        center_freq_hz: center,
        tone_freq_abs_hz: center + toneOffsetKHz * 1000,
        amplitude,
      },
    });
    open = false;
    onClose();
  }

  function pickDevice(caps: DeviceCapabilities) {
    onPickDevice(caps);
    open = false;
    onClose();
  }

  /** One-line summary of the active source — populates the "Selected"
   *  header row so the dialog is self-locating ("yes, the right SDR is
   *  mounted"). Reads the SourceConfig's type + a few diagnostic
   *  params; falls back to a literal "—" when no source is set yet. */
  function selectedSummary(cfg: SourceConfig | null): { kind: string; detail: string } {
    if (!cfg) return { kind: 'none', detail: '—' };
    switch (cfg.type) {
      case 'SoapySource': {
        const args = (cfg.params.args as string | undefined) ?? '';
        // Parse `key=value,key=value` to pull out the human-friendly bits.
        // RTL-SDR generic dongles expose no `label` key — just
        // `driver=rtlsdr` — so falling back twice to driver gave us
        // "rtlsdr · rtlsdr". Collapse the duplicate, and when parsing
        // produces nothing usable (no driver key found, e.g. a
        // hand-edited bare args string) show the raw args verbatim so
        // the operator can see what they configured.
        const parts = new Map<string, string>();
        for (const seg of args.split(',')) {
          const eq = seg.indexOf('=');
          if (eq > 0) parts.set(seg.slice(0, eq).trim(), seg.slice(eq + 1).trim());
        }
        const driver = parts.get('driver');
        const label = parts.get('label');
        if (label && driver && label !== driver) {
          return { kind: 'SDR', detail: `${label} · ${driver}` };
        }
        if (label) return { kind: 'SDR', detail: label };
        if (driver) return { kind: 'SDR', detail: driver };
        return { kind: 'SDR', detail: args || '—' };
      }
      case 'SineSource': {
        const cHz = (cfg.params.center_freq_hz as number | undefined) ?? 0;
        const rHz = (cfg.params.rate_hz as number | undefined) ?? 0;
        return {
          kind: 'Tone',
          detail: `${(cHz / 1e6).toFixed(3)} MHz @ ${(rHz / 1e6).toFixed(1)} MS/s`,
        };
      }
      case 'FileSource': {
        const path = (cfg.params.path as string | undefined) ?? '';
        const base = path.split('/').pop() ?? path;
        return { kind: 'File', detail: base };
      }
      default:
        return { kind: cfg.type, detail: '' };
    }
  }

  let summary = $derived(selectedSummary(source));
</script>

<Dialog.Root bind:open onOpenChange={(o) => !o && onClose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="fixed inset-0 z-40 bg-black/50" />
    <Dialog.Content
      class="fixed left-1/2 top-1/2 z-50 w-[26rem] -translate-x-1/2 -translate-y-1/2 rounded-md border border-slate-800 bg-[color:var(--color-bg)] text-[color:var(--color-fg)] shadow-xl"
    >
      <div class="flex flex-col">
        <!-- Header: title, terse subtitle, close. -->
        <div
          class="flex items-baseline justify-between gap-3 border-b border-slate-800 px-3 py-1.5"
        >
          <Dialog.Title class="text-sm font-semibold">Source</Dialog.Title>
          <Dialog.Description
            class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            which SDR this app sees
          </Dialog.Description>
        </div>

        <!-- Selected — current source identity in one line. The
             "params" surface (gain / antenna / agc) lives in the live
             Settings panel; keeping it out of here avoids duplicating
             knobs. -->
        <div class="flex items-baseline gap-3 border-b border-slate-800 px-3 py-1.5 text-xs">
          <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
            >Selected</span
          >
          <span class="truncate font-mono" title={summary.detail}>{summary.detail}</span>
          <span class="ml-auto text-[10px] text-[color:var(--color-muted)]">{summary.kind}</span>
        </div>

        <!-- Body: device list is the dialog's job. Capped by max-h so
             a long list scrolls without resizing the modal; no min-h
             — when empty/single-device the dialog stays compact. -->
        <div class="max-h-[28rem] overflow-y-auto px-3 py-2">
          <DevicePicker onSelect={pickDevice} />

          <!-- Other sources: tone / file / json. Collapsed by default;
               opens to three sub-disclosures. Hardware is the main
               job, so these get a single trigger line until needed. -->
          <details class="mt-3 border-t border-slate-800 pt-2">
            <summary
              class="cursor-pointer list-none text-[10px] uppercase tracking-wide text-[color:var(--color-muted)] hover:text-slate-200"
            >
              Other sources ▸
            </summary>
            <div class="mt-2 flex flex-col gap-1">
              <details class="rounded border border-slate-800">
                <summary
                  class="cursor-pointer list-none px-2 py-1 text-xs text-[color:var(--color-muted)] hover:text-slate-200"
                  title="Built-in sine — no hardware needed, useful for verifying the pipeline."
                >
                  Tone (built-in sine)
                </summary>
                <form
                  class="flex flex-col gap-3 px-2 pb-2 text-xs"
                  onsubmit={(e) => {
                    e.preventDefault();
                    applyTone();
                  }}
                >
                  <label class="grid gap-1">
                    <div class="flex justify-between">
                      <span class="text-[color:var(--color-muted)]">Centre</span>
                      <span class="font-mono">{centerFreqMHz.toFixed(3)} MHz</span>
                    </div>
                    <input
                      type="number"
                      step="0.001"
                      min="0"
                      bind:value={centerFreqMHz}
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-0.5"
                    />
                  </label>
                  <label class="grid gap-1">
                    <div class="flex justify-between">
                      <span class="text-[color:var(--color-muted)]">Sample rate</span>
                      <span class="font-mono">{rateMHz.toFixed(3)} MHz</span>
                    </div>
                    <input
                      type="number"
                      step="0.1"
                      min="0.1"
                      bind:value={rateMHz}
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-0.5"
                    />
                  </label>
                  <label class="grid gap-1">
                    <div class="flex justify-between">
                      <span class="text-[color:var(--color-muted)]">Tone offset</span>
                      <span class="font-mono">
                        {toneOffsetKHz >= 0 ? '+' : '−'}{Math.abs(toneOffsetKHz)} kHz
                      </span>
                    </div>
                    <input type="range" min="-900" max="900" step="1" bind:value={toneOffsetKHz} />
                  </label>
                  <label class="grid gap-1">
                    <div class="flex justify-between">
                      <span class="text-[color:var(--color-muted)]">Amplitude</span>
                      <span class="font-mono">{amplitude.toFixed(2)}</span>
                    </div>
                    <input type="range" min="0" max="1" step="0.01" bind:value={amplitude} />
                  </label>
                  <div class="flex justify-end">
                    <button
                      type="submit"
                      class="rounded bg-[color:var(--color-accent)] px-2 py-0.5 text-xs font-semibold text-slate-900"
                    >
                      Apply Tone
                    </button>
                  </div>
                </form>
              </details>

              <details class="rounded border border-slate-800">
                <summary
                  class="cursor-pointer list-none px-2 py-1 text-xs text-[color:var(--color-muted)] hover:text-slate-200"
                  title="Replay a recorded IQ/audio capture — dev/debug."
                >
                  File (replay capture)
                </summary>
                <div class="px-2 pb-2">
                  <SamplePicker
                    onApply={(cfg) => {
                      onApply(cfg);
                      open = false;
                      onClose();
                    }}
                    onCancel={() => {
                      open = false;
                      onClose();
                    }}
                  />
                </div>
              </details>

              <details class="rounded border border-slate-800">
                <summary
                  class="cursor-pointer list-none px-2 py-1 text-xs text-[color:var(--color-muted)] hover:text-slate-200"
                  title="Composed preset JSON — read-only inspection."
                >
                  JSON (read-only)
                </summary>
                <div class="px-2 pb-2">
                  <PresetJsonView refreshKey={open} />
                </div>
              </details>
            </div>
          </details>
        </div>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
