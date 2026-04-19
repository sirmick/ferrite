<script lang="ts">
  import { Dialog } from 'bits-ui';
  import DevicePicker from './DevicePicker.svelte';
  import type { OpenDeviceRequest } from '$lib/api/device';
  import type { DeviceCapabilities } from '$lib/api/devices';
  import { untrack } from 'svelte';

  interface Props {
    open: boolean;
    /** Current tone/source params — seeds the Tone tab. */
    params: OpenDeviceRequest;
    /** Selected a Soapy device: parent should open the DeviceOptions flow. */
    onPickDevice: (caps: DeviceCapabilities) => void;
    /** Applied the tone-generator form: parent should (re)open the session. */
    onApplyTone: (next: OpenDeviceRequest) => void;
    onClose: () => void;
  }

  let { open = $bindable(), params, onPickDevice, onApplyTone, onClose }: Props = $props();

  type Tab = 'device' | 'tone' | 'file';
  let tab = $state<Tab>('device');

  const CENTER_FREQ_HZ = 100_000_000;

  function toneOffsetFrom(p: OpenDeviceRequest): number {
    const abs = p.tone_freq_abs_hz ?? CENTER_FREQ_HZ + 1000;
    return Math.round((abs - CENTER_FREQ_HZ) / 1000);
  }

  let toneOffsetKHz = $state(untrack(() => toneOffsetFrom(params)));
  let amplitude = $state(untrack(() => params.amplitude ?? 0.25));
  let fftSize = $state(untrack(() => params.fft_size ?? 4096));
  let fftRateHz = $state(untrack(() => params.fft_rate_hz ?? 30));

  $effect(() => {
    if (open) {
      toneOffsetKHz = toneOffsetFrom(params);
      amplitude = params.amplitude ?? 0.25;
      fftSize = params.fft_size ?? 4096;
      fftRateHz = params.fft_rate_hz ?? 30;
    }
  });

  function applyTone() {
    onApplyTone({
      sample_rate_hz: 2_000_000,
      center_freq_hz: CENTER_FREQ_HZ,
      tone_freq_abs_hz: CENTER_FREQ_HZ + toneOffsetKHz * 1000,
      amplitude,
      fft_size: fftSize,
      fft_rate_hz: fftRateHz,
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

              <label class="grid gap-1 text-xs">
                <span class="text-[color:var(--color-muted)]">FFT size</span>
                <select
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                  bind:value={fftSize}
                >
                  <option value={512}>512</option>
                  <option value={1024}>1024</option>
                  <option value={2048}>2048</option>
                  <option value={4096}>4096</option>
                  <option value={8192}>8192</option>
                </select>
              </label>

              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">FFT rate</span>
                  <span class="font-mono">{fftRateHz} Hz</span>
                </div>
                <input type="range" min="5" max="60" step="1" bind:value={fftRateHz} />
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
                class="rounded border border-slate-800 bg-slate-900/60 p-2 font-mono text-[11px] text-slate-300">cargo run -p ferrited -- --source file:///path/to/capture.cf32 --rate 2000000 --freq 100000000</pre>
              <p>REST-side file browsing lands in a later pass.</p>
            </div>
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
