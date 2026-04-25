<script lang="ts">
  // At-a-glance health widget — three coloured dots for the chain that
  // matters in this app: SDR source producing samples, server-side
  // scheduler ticking, browser-side wasm scheduler ticking. Each dot
  // surfaces a hover tooltip with a one-liner explanation, so a quick
  // glance shows go/no-go and a hover answers "why".
  //
  // Data comes from `flow.node` / `flow.browser` snapshots (the same
  // 1 Hz `flowdiag` stream the Flow tab uses) plus `pipeline.status`.
  // Dots stay grey when the pipeline isn't running — there's nothing
  // wrong, there's just nothing to indicate.

  import { pipeline } from '$lib/pipeline.svelte';
  import { flow } from '$lib/logs/flowStore.svelte';

  const FRESH_MS = 2_000;
  const STALE_MS = 5_000;

  function ageMs(at: number | undefined | null): number {
    if (!at) return Number.POSITIVE_INFINITY;
    return Date.now() - at;
  }

  // Tick the timestamp every 500 ms so the widget reflects a stalled
  // schedule even when no new flowdiag arrives.
  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 500);
    return () => clearInterval(t);
  });

  let running = $derived(pipeline.status === 'running');

  type DotState = 'ok' | 'stale' | 'down' | 'idle';

  function freshness(side: 'node' | 'browser'): DotState {
    if (!running) return 'idle';
    const at = side === 'node' ? flow.node?.lastUpdate : flow.browser?.lastUpdate;
    const dt = ageMs(at) - (now - Date.now()); // align with `now` reactive
    if (!at) return 'down';
    if (dt < FRESH_MS) return 'ok';
    if (dt < STALE_MS) return 'stale';
    return 'down';
  }

  // Source health: any node-side block has at least one output port
  // moving samples per second. Covers Source/Channelizer/etc. without
  // needing to know the exact block type or id of the source.
  function sourceHealth(): DotState {
    if (!running) return 'idle';
    const node = flow.node;
    if (!node || ageMs(node.lastUpdate) > STALE_MS) return 'down';
    const anyMoving = node.blocks.some((b) => b.outputs.some((p) => p.sps > 0));
    if (anyMoving) return 'ok';
    return 'stale';
  }

  let _now = $derived(now); // pull `now` into reactive deps
  let srcDot = $derived(
    (() => {
      void _now;
      return sourceHealth();
    })(),
  );
  let nodeDot = $derived(
    (() => {
      void _now;
      return freshness('node');
    })(),
  );
  let browserDot = $derived(
    (() => {
      void _now;
      return freshness('browser');
    })(),
  );

  function colour(s: DotState): string {
    switch (s) {
      case 'ok':
        return '#34d399'; // emerald
      case 'stale':
        return '#facc15'; // amber
      case 'down':
        return '#f87171'; // rose
      case 'idle':
      default:
        return '#475569'; // slate-600
    }
  }

  function tooltip(label: string, s: DotState): string {
    const verb =
      s === 'ok'
        ? 'healthy'
        : s === 'stale'
          ? 'stalled'
          : s === 'down'
            ? 'no signal'
            : 'idle (not running)';
    return `${label}: ${verb}`;
  }
</script>

<div class="flex items-center gap-1.5" aria-label="pipeline health">
  <span class="dot" style:background-color={colour(srcDot)} title={tooltip('input source', srcDot)}
  ></span>
  <span
    class="dot"
    style:background-color={colour(nodeDot)}
    title={tooltip('server scheduler', nodeDot)}
  ></span>
  <span
    class="dot"
    style:background-color={colour(browserDot)}
    title={tooltip('browser scheduler', browserDot)}
  ></span>
</div>

<style>
  .dot {
    display: inline-block;
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 9999px;
    box-shadow: 0 0 0 1px rgba(255, 255, 255, 0.05);
  }
</style>
