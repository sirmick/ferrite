<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import { demoAddInWorker } from '$lib/workers/demo-client';
  import { closeDevice, openDevice, wsUrlFor } from '$lib/api/device';
  import { FrameClient, type ClientStatus } from '$lib/ws/client';
  import { FFT_STREAM, type ParsedFrame } from '$lib/ws/frame';
  import { onMount } from 'svelte';

  let wasmStatus = $state<'pending' | 'ok' | string>('pending');
  let wsStatus = $state<ClientStatus | 'idle' | string>('idle');
  let frameRate = $state(0);
  let lastFrameSize = $state(0);

  async function runDemo() {
    try {
      const { sum } = await demoAddInWorker(1.5, 2.25);
      wasmStatus = Math.abs(sum - 3.75) < 1e-6 ? 'ok' : `wrong: ${sum}`;
    } catch (err) {
      wasmStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  onMount(() => {
    let client: FrameClient | undefined;
    let sessionId: string | undefined;
    let counter = 0;
    let windowStart = performance.now();
    let rateTimer: ReturnType<typeof setInterval> | undefined;
    let cancelled = false;

    (async () => {
      try {
        const opened = await openDevice({ fft_size: 4096, fft_rate_hz: 30 });
        if (cancelled) {
          await closeDevice(opened.session_id).catch(() => {});
          return;
        }
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
        client.subscribe(FFT_STREAM, (frame: ParsedFrame) => {
          counter += 1;
          lastFrameSize = frame.payload.length;
        });
        rateTimer = setInterval(() => {
          const now = performance.now();
          const dt = (now - windowStart) / 1000;
          frameRate = dt > 0 ? counter / dt : 0;
          counter = 0;
          windowStart = now;
        }, 1000);
      } catch (err) {
        wsStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
      }
    })();

    return () => {
      cancelled = true;
      if (rateTimer !== undefined) clearInterval(rateTimer);
      client?.close();
      if (sessionId) void closeDevice(sessionId).catch(() => {});
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
    </div>
  </header>
  <div class="min-h-0 flex-1">
    <Workspace />
  </div>
</div>
