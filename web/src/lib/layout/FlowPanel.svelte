<script lang="ts">
  // Live per-block flow table. Consumes `flow` store state populated by
  // the 1 Hz `flowdiag` lines coming off the runner (browser) and
  // ferrited (node). One row per block, per side; columns show the
  // per-port sample rate (sps), ring depth at snapshot time, process
  // time as % of wall-clock, and process-call count in the window.
  //
  // Renders both sides in order: node half first (server), then browser
  // half. Each section collapses independently. When a side has no
  // data yet, shows a "waiting for flowdiag…" placeholder so the user
  // knows the plumbing is wired but the pipeline hasn't started.

  import { flow, type FlowSide } from '$lib/logs/flowStore.svelte';

  // The node-side snapshot comes over its own channel (GET
  // /api/flowdiag), not the log stream. The 1 Hz poll lives at the
  // store layer (`flowStore.svelte.ts`) so it runs whenever the app
  // is up — not just when this panel is mounted. Gating it on
  // FlowPanel visibility broke `HealthDots`, which depends on the
  // freshness of `flow.node` regardless of which tab is open.
  // The browser side is fed directly from the runner; nothing to
  // poll there.

  function fmtSps(n: number): string {
    if (!Number.isFinite(n) || n === 0) return '—';
    if (n >= 1e6) return `${(n / 1e6).toFixed(2)} MS/s`;
    if (n >= 1e3) return `${(n / 1e3).toFixed(1)} kS/s`;
    return `${Math.round(n)} S/s`;
  }
  function fmtPct(n: number): string {
    if (!Number.isFinite(n) || n < 0.05) return '—';
    return `${n.toFixed(1)}%`;
  }
  function fmtBuf(n: number): string {
    if (n === 0) return '—';
    if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
    if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
    return `${n}`;
  }

  // Side rows get a tiny CSS-grid so port names align across columns.
  // Block row renders the block's own counters on the first line; each
  // port gets its own sub-row indented underneath.
</script>

<div class="flex h-full w-full flex-col overflow-y-auto bg-[color:var(--color-bg)] text-xs">
  {#each [['node', flow.node], ['browser', flow.browser]] as [label, side] (label)}
    <section class="border-b border-slate-900">
      <header
        class="flex items-baseline justify-between border-b border-slate-800 px-2 py-1 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
      >
        <span>{label}</span>
        {#if side}
          <span class="text-[9px] text-slate-600">
            {(side as FlowSide).blocks.length} blocks · {(side as FlowSide).windowMs} ms window
          </span>
        {/if}
      </header>
      {#if !side}
        <p class="p-3 text-[10px] text-slate-600">waiting for flowdiag…</p>
      {:else}
        <div
          class="grid grid-cols-[auto_1fr_auto_auto_auto] gap-x-3 gap-y-0.5 px-2 py-1 font-mono text-[10px]"
        >
          <span class="text-[color:var(--color-muted)]">block</span>
          <span class="text-[color:var(--color-muted)]">port</span>
          <span class="text-right text-[color:var(--color-muted)]">sps</span>
          <span class="text-right text-[color:var(--color-muted)]">buf</span>
          <span class="text-right text-[color:var(--color-muted)]">cpu%</span>
          {#each (side as FlowSide).blocks as b (b.id)}
            <!-- header row: block id + process% -->
            <span class="col-span-2 pt-0.5 text-slate-200">
              {b.id}
              <span class="text-[9px] text-slate-600">({b.typeName})</span>
            </span>
            <span class="pt-0.5 text-right text-slate-600">—</span>
            <span class="pt-0.5 text-right text-slate-600">—</span>
            <span class="pt-0.5 text-right text-slate-300">{fmtPct(b.processPct)}</span>
            {#each b.inputs as p (`in-${p.name}`)}
              <span></span>
              <span class="text-[color:var(--color-muted)]">↳ in/{p.name}</span>
              <span class="text-right text-slate-300">{fmtSps(p.sps)}</span>
              <span class="text-right text-slate-400">{fmtBuf(p.buffered)}</span>
              <span></span>
            {/each}
            {#each b.outputs as p (`out-${p.name}`)}
              <span></span>
              <span class="text-[color:var(--color-muted)]">↳ out/{p.name}</span>
              <span class="text-right text-slate-300">{fmtSps(p.sps)}</span>
              <span></span>
              <span></span>
            {/each}
          {/each}
        </div>
      {/if}
    </section>
  {/each}
</div>
