<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import SourceModal from '$lib/controls/SourceModal.svelte';
  import { demoAddInWorker } from '$lib/workers/demo-client';
  import { closeDevice, openDevice, wsUrlFor, type OpenDeviceRequest } from '$lib/api/device';
  import { FrameClient, type ClientStatus } from '$lib/ws/client';
  import { FFT_STREAM } from '$lib/ws/frame';
  import { onMount } from 'svelte';

  let wasmStatus = $state<'pending' | 'ok' | string>('pending');
  let wsStatus = $state<ClientStatus | 'idle' | string>('idle');
  let frameRate = $state(0);
  let lastFrameSize = $state(0);
  let client = $state<FrameClient | undefined>(undefined);
  let sessionId: string | undefined;
  let showSource = $state(false);

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

  async function teardown() {
    const c = client;
    const s = sessionId;
    client = undefined;
    sessionId = undefined;
    if (c) c.close();
    if (s) await closeDevice(s).catch(() => {});
  }

  async function connect(next: OpenDeviceRequest) {
    await teardown();
    wsStatus = 'connecting';
    try {
      const opened = await openDevice(next);
      sessionId = opened.session_id;
      client = new FrameClient({
        url: wsUrlFor(opened.ws_url),
        onStatus: (s) => {
          wsStatus = s;
        },
        onDecodeError: (err) => {
          wsStatus = `decode error: ${err.message}`;
        },
      });
      params = next;
    } catch (err) {
      wsStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  onMount(() => {
    void connect($state.snapshot(params));
    return () => {
      void teardown();
    };
  });

  // Count frames per second against whichever client is current; a re-open
  // swaps `client`, this effect re-subscribes automatically.
  $effect(() => {
    const c = client;
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
        ws: {wsStatus}
        {#if wsStatus === 'open' && frameRate > 0}
          ({frameRate.toFixed(1)} fps, {lastFrameSize} B)
        {/if}
      </span>
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
    {#if client}
      <Workspace {client} />
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
  onApply={(p) => void connect(p)}
/>
