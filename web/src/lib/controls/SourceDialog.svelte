<script lang="ts">
  import { Dialog } from 'bits-ui';
  import DevicePicker from './DevicePicker.svelte';
  import PresetJsonView from './PresetJsonView.svelte';
  import type { SourceConfig } from '$lib/api/source';
  import type { DeviceCapabilities } from '$lib/api/devices';
  import { untrack } from 'svelte';

  interface Props {
    open: boolean;
    /** Current source config — seeds the Tone tab when its type is sine. */
    source: SourceConfig | null;
    /** Selected a Soapy device: parent applies a default SourceConfig. */
    onPickDevice: (caps: DeviceCapabilities) => void;
    /** Applied a source-config from any tab — parent PATCHes it. */
    onApply: (cfg: SourceConfig) => void;
    onClose: () => void;
  }

  let { open = $bindable(), source, onPickDevice, onApply, onClose }: Props = $props();

  type Tab = 'device' | 'tone' | 'file' | 'json';
  let tab = $state<Tab>('device');

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

  const TABS: { id: Tab; label: string }[] = [
    { id: 'device', label: 'Device' },
    { id: 'tone', label: 'Tone' },
    { id: 'file', label: 'File' },
    { id: 'json', label: 'JSON' },
  ];
</script>

<Dialog.Root bind:open onOpenChange={(o) => !o && onClose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="fixed inset-0 z-40 bg-black/50" />
    <Dialog.Content
      class="fixed left-1/2 top-1/2 z-50 w-[34rem] -translate-x-1/2 -translate-y-1/2 rounded-md border border-slate-800 bg-[color:var(--color-bg)] text-[color:var(--color-fg)] shadow-xl"
    >
      <div class="flex flex-col">
        <div class="flex items-baseline justify-between border-b border-slate-800 px-4 py-2">
          <Dialog.Title class="text-sm font-semibold">Source</Dialog.Title>
          <Dialog.Description
            class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            pick a signal source
          </Dialog.Description>
        </div>

        <div class="flex border-b border-slate-800">
          {#each TABS as t (t.id)}
            <button
              type="button"
              class="px-4 py-2 text-xs"
              class:tab-active={tab === t.id}
              class:tab-idle={tab !== t.id}
              onclick={() => (tab = t.id)}
            >
              {t.label}
            </button>
          {/each}
        </div>

        <div class="max-h-[32rem] min-h-[16rem] overflow-y-auto p-4">
          {#if tab === 'device'}
            <DevicePicker onSelect={pickDevice} />
          {:else if tab === 'tone'}
            <form
              class="flex flex-col gap-4"
              onsubmit={(e) => {
                e.preventDefault();
                applyTone();
              }}
            >
              <p class="text-xs text-[color:var(--color-muted)]">
                Built-in sine wave — useful for verifying the pipeline without hardware.
              </p>

              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Centre frequency</span>
                  <span class="font-mono">{centerFreqMHz.toFixed(3)} MHz</span>
                </div>
                <input
                  type="number"
                  step="0.001"
                  min="0"
                  bind:value={centerFreqMHz}
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                />
              </label>

              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Sample rate</span>
                  <span class="font-mono">{rateMHz.toFixed(3)} MHz</span>
                </div>
                <input
                  type="number"
                  step="0.1"
                  min="0.1"
                  bind:value={rateMHz}
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                />
              </label>

              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Tone offset</span>
                  <span class="font-mono">
                    {toneOffsetKHz >= 0 ? '+' : '−'}{Math.abs(toneOffsetKHz)} kHz
                  </span>
                </div>
                <input type="range" min="-900" max="900" step="1" bind:value={toneOffsetKHz} />
              </label>

              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Amplitude</span>
                  <span class="font-mono">{amplitude.toFixed(2)}</span>
                </div>
                <input type="range" min="0" max="1" step="0.01" bind:value={amplitude} />
              </label>

              <div class="flex justify-end gap-2 pt-2">
                <button
                  type="button"
                  class="rounded border border-slate-700 px-3 py-1 text-sm"
                  onclick={() => {
                    open = false;
                    onClose();
                  }}
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  class="rounded bg-[color:var(--color-accent)] px-3 py-1 text-sm font-semibold text-slate-900"
                >
                  Apply
                </button>
              </div>
            </form>
          {:else if tab === 'file'}
            <div class="flex flex-col gap-3 text-xs text-[color:var(--color-muted)]">
              <p>IQ file replay is wired as a CLI option today:</p>
              <pre
                class="rounded border border-slate-800 bg-slate-900/60 p-2 font-mono text-[11px] text-slate-300">cargo run -p ferrited -- --flowgraph flowgraphs/wbfm.json --source-type FileSource --source-path /path/to/capture.cf32</pre>
              <p>REST-side file browsing lands in a later pass.</p>
            </div>
          {:else if tab === 'json'}
            <PresetJsonView refreshKey={open} />
          {/if}
        </div>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>

<style>
  .tab-active {
    border-bottom: 2px solid var(--color-accent);
    color: var(--color-fg);
    font-weight: 600;
  }
  .tab-idle {
    color: var(--color-muted);
    border-bottom: 2px solid transparent;
  }
  .tab-idle:hover {
    color: var(--color-fg);
  }
</style>
