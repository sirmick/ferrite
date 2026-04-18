<script lang="ts">
  import { untrack } from 'svelte';
  import type { OpenDeviceRequest } from '$lib/api/device';

  interface Props {
    open: boolean;
    params: OpenDeviceRequest;
    onApply: (next: OpenDeviceRequest) => void;
    onClose: () => void;
  }

  let { open, params, onApply, onClose }: Props = $props();

  // Fixed for now; later commits add device selection + freq tuning.
  const SAMPLE_RATE_HZ = 2_000_000;
  const CENTER_FREQ_HZ = 100_000_000;

  function toneOffsetFrom(p: OpenDeviceRequest): number {
    const abs = p.tone_freq_abs_hz ?? CENTER_FREQ_HZ + 1000;
    return Math.round((abs - CENTER_FREQ_HZ) / 1000);
  }

  let toneOffsetKHz = $state(untrack(() => toneOffsetFrom(params)));
  let amplitude = $state(untrack(() => params.amplitude ?? 0.25));
  let fftSize = $state(untrack(() => params.fft_size ?? 4096));
  let fftRateHz = $state(untrack(() => params.fft_rate_hz ?? 30));

  // Re-sync local form when the external `params` prop changes (e.g. after a
  // successful re-open). Without this the modal would show stale values next
  // time it's opened.
  $effect(() => {
    toneOffsetKHz = toneOffsetFrom(params);
    amplitude = params.amplitude ?? 0.25;
    fftSize = params.fft_size ?? 4096;
    fftRateHz = params.fft_rate_hz ?? 30;
  });

  let dialog: HTMLDialogElement | undefined = $state();
  $effect(() => {
    const d = dialog;
    if (!d) return;
    if (open && !d.open) d.showModal();
    else if (!open && d.open) d.close();
  });

  function apply() {
    onApply({
      sample_rate_hz: SAMPLE_RATE_HZ,
      center_freq_hz: CENTER_FREQ_HZ,
      tone_freq_abs_hz: CENTER_FREQ_HZ + toneOffsetKHz * 1000,
      amplitude,
      fft_size: fftSize,
      fft_rate_hz: fftRateHz,
    });
    onClose();
  }
</script>

<dialog
  bind:this={dialog}
  onclose={onClose}
  class="w-96 rounded-md border border-slate-800 bg-[color:var(--color-bg)] p-0 text-[color:var(--color-fg)]"
>
  <form
    class="flex flex-col gap-4 p-4"
    onsubmit={(e) => {
      e.preventDefault();
      apply();
    }}
  >
    <div class="flex items-baseline justify-between">
      <h2 class="text-sm font-semibold">Source</h2>
      <span class="text-xs text-[color:var(--color-muted)]">Sine (built-in)</span>
    </div>

    <label class="grid gap-1 text-xs">
      <div class="flex justify-between">
        <span class="text-[color:var(--color-muted)]">Tone offset</span>
        <span class="font-mono">{toneOffsetKHz.toFixed(0)} kHz</span>
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
      <select class="rounded border border-slate-800 bg-slate-900 px-2 py-1" bind:value={fftSize}>
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
        onclick={onClose}
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
</dialog>

<style>
  dialog::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }
</style>
