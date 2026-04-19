<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import SourceModal from '$lib/controls/SourceModal.svelte';
  import DevicePicker from '$lib/controls/DevicePicker.svelte';
  import DeviceOptions from '$lib/controls/DeviceOptions.svelte';
  import type { DeviceCapabilities } from '$lib/api/devices';
  import { demoAddInWorker } from '$lib/workers/demo-client';
  import type { OpenDeviceRequest } from '$lib/api/device';
  import { FFT_STREAM } from '$lib/ws/frame';
  import { session } from '$lib/session.svelte';
  import { onMount } from 'svelte';

  let wasmStatus = $state<'pending' | 'ok' | string>('pending');
  let frameRate = $state(0);
  let lastFrameSize = $state(0);
  let showSource = $state(false);
  let showDevices = $state(false);
  let devicesDialog: HTMLDialogElement | undefined = $state();
  $effect(() => {
    const d = devicesDialog;
    if (!d) return;
    if (showDevices && !d.open) d.showModal();
    else if (!showDevices && d.open) d.close();
  });
  let showOptions = $state(false);
  let optionsCaps = $state<DeviceCapabilities | null>(null);

  // Initial sine-source defaults the SourceModal seeds from. The session
  // store carries the live spec once a device is open; this is just the
  // first-load form state.
  let params = $state<OpenDeviceRequest>({
    sample_rate_hz: 2_000_000,
    center_freq_hz: 100_000_000,
    tone_freq_abs_hz: 100_001_000,
    amplitude: 0.25,
    fft_size: 4096,
    fft_rate_hz: 30,
  });

  async function runDemo() {
    try {
      const { sum } = await demoAddInWorker(1.5, 2.25);
      wasmStatus = Math.abs(sum - 3.75) < 1e-6 ? 'ok' : `wrong: ${sum}`;
    } catch (err) {
      wasmStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  onMount(() => {
    void session.open($state.snapshot(params));
    return () => {
      void session.teardown();
    };
  });

  // Count frames per second against whichever client is current; a re-open
  // swaps `session.client`, this effect re-subscribes automatically.
  $effect(() => {
    const c = session.client;
    if (!c) {
      frameRate = 0;
      return;
    }
    let counter = 0;
    let windowStart = performance.now();
    const unsub = c.subscribe(FFT_STREAM, (frame) => {
      counter += 1;
      lastFrameSize = frame.payload.length;
    });
    const timer = setInterval(() => {
      const now = performance.now();
      const dt = (now - windowStart) / 1000;
      frameRate = dt > 0 ? counter / dt : 0;
      counter = 0;
      windowStart = now;
    }, 1000);
    return () => {
      clearInterval(timer);
      unsub();
    };
  });

  $effect(() => {
    void runDemo();
  });

  // Pretty-print the active source for the header chip.
  let sourceLabel = $derived.by(() => {
    const s = session.state;
    if (!s) return '';
    if (s.source_kind === 'soapy') return `soapy ${(s.center_freq_hz / 1e6).toFixed(3)} MHz`;
    if (s.source_kind === 'file') return 'file';
    return `sine ${(s.center_freq_hz / 1e6).toFixed(3)} MHz`;
  });
</script>

<div class="flex h-dvh w-dvw flex-col">
  <header class="flex items-center justify-between border-b border-slate-800 px-4 py-2">
    <div class="flex items-baseline gap-3">
      <h1 class="text-lg font-semibold">Ferrite</h1>
      <span class="text-xs text-[color:var(--color-muted)]">pre-alpha</span>
    </div>
    <div class="flex items-center gap-4 text-xs text-[color:var(--color-muted)]">
      <span>wasm: {wasmStatus}</span>
      <span>
        ws: {session.wsStatus}
        {#if session.wsStatus === 'open' && frameRate > 0}
          ({frameRate.toFixed(1)} fps, {lastFrameSize} B)
        {/if}
      </span>
      {#if sourceLabel}
        <span class="font-mono text-[11px]">{sourceLabel}</span>
      {/if}
      {#if session.errorMessage}
        <span class="text-rose-400" title={session.errorMessage}>error</span>
      {/if}
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-xs text-[color:var(--color-fg)] hover:border-slate-600"
        onclick={() => (showDevices = true)}
      >
        Devices…
      </button>
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-xs text-[color:var(--color-fg)] hover:border-slate-600"
        onclick={() => (showSource = true)}
      >
        Source…
      </button>
    </div>
  </header>
  <div class="min-h-0 flex-1">
    {#if session.client}
      <Workspace client={session.client} />
    {:else}
      <div class="flex h-full items-center justify-center text-sm text-[color:var(--color-muted)]">
        waiting for session…
      </div>
    {/if}
  </div>
</div>

<SourceModal
  open={showSource}
  {params}
  onClose={() => (showSource = false)}
  onApply={(p) => {
    params = p;
    void session.open(p);
  }}
/>

<dialog
  bind:this={devicesDialog}
  onclose={() => (showDevices = false)}
  class="w-[28rem] rounded-md border border-slate-800 bg-[color:var(--color-bg)] p-0 text-[color:var(--color-fg)]"
>
  <div class="flex flex-col gap-3 p-4">
    <DevicePicker
      onSelect={(caps) => {
        showDevices = false;
        optionsCaps = caps;
        showOptions = true;
      }}
    />
    <div class="flex justify-end">
      <button
        type="button"
        class="rounded border border-slate-700 px-3 py-1 text-sm"
        onclick={() => (showDevices = false)}
      >
        Close
      </button>
    </div>
  </div>
</dialog>

<DeviceOptions
  bind:open={showOptions}
  capabilities={optionsCaps}
  onApply={(req) => void session.open(req)}
  onClose={() => (showOptions = false)}
/>

<style>
  dialog::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }
</style>
