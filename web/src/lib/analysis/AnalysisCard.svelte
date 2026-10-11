<script lang="ts">
  // One `analysis` record (server/src/analysis.rs), drawn inline in the AI
  // panel — whoever ran it (the AI over MCP, a person on the CLI, the UI).
  // The record carries structured results; this draws them. `text` (the
  // character rendering the agent sees) is one toggle away.
  import type { AnalysisRecord, Peak } from './types';
  import { decodeCells, heat } from './types';

  let { rec, t }: { rec: AnalysisRecord; t: number } = $props();
  let showText = $state(false);

  const mhz = (f: number) => (f / 1e6).toFixed(5);
  const byLabel = $derived(rec.by === 'mcp' ? 'AI' : rec.by === 'cli' ? 'CLI' : 'UI');
  const title = $derived.by(() => {
    const p = rec.params ?? {};
    if (rec.tool === 'envelope') return `envelope @ ${mhz(p.freq_hz ?? 0)} MHz`;
    return `${rec.tool} ${mhz(p.lo_hz ?? 0)}–${mhz(p.hi_hz ?? 0)} MHz`;
  });

  // ── spectrum ──────────────────────────────────────────────────────
  const W = 320;
  const H = 90;
  const spec = $derived.by(() => {
    if (rec.tool !== 'spectrum') return null;
    const prof: [number, number][] = rec.result?.profile ?? [];
    if (prof.length < 2 && !(rec.result?.peaks ?? []).length) return null;
    const lo = rec.params?.lo_hz ?? prof[0]?.[0] ?? 0;
    const hi = rec.params?.hi_hz ?? prof[prof.length - 1]?.[0] ?? lo + 1;
    const top = Math.max(12, ...prof.map((p) => p[1]));
    const x = (f: number) => ((f - lo) / (hi - lo)) * W;
    const y = (db: number) => H - (Math.max(0, db) / top) * (H - 4);
    const line = prof.map((p) => `${x(p[0]).toFixed(1)},${y(p[1]).toFixed(1)}`).join(' ');
    const peaks = (rec.result?.peaks ?? []).slice(0, 12) as Peak[];
    return { line, peaks, x, y, lo, hi };
  });

  async function tuneTo(f: number) {
    await fetch('/api/tune', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ carrier_hz: f, keep_lo: true }),
    }).catch(() => {});
  }

  // ── spectrogram ───────────────────────────────────────────────────
  let canvas: HTMLCanvasElement | undefined = $state();
  $effect(() => {
    if (rec.tool !== 'spectrogram' || !canvas || !rec.cells_b64) return;
    const { rows, cols } = rec.result ?? { rows: 0, cols: 0 };
    const cells = decodeCells(rec.cells_b64);
    if (!rows || !cols || cells.length < rows * cols) return;
    canvas.width = cols;
    canvas.height = rows;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const img = ctx.createImageData(cols, rows);
    for (let i = 0; i < rows * cols; i++) {
      const [r, g, b] = heat(cells[i]);
      img.data[i * 4] = r;
      img.data[i * 4 + 1] = g;
      img.data[i * 4 + 2] = b;
      img.data[i * 4 + 3] = 255;
    }
    ctx.putImageData(img, 0, 0);
  });

  // ── envelope ──────────────────────────────────────────────────────
  // Thousands of 10 ms windows → one bar per pixel column (max), so keying
  // stays readable at any duration; on-segments shade behind.
  const env = $derived.by(() => {
    if (rec.tool !== 'envelope') return null;
    const p: number[] = rec.result?.power_db ?? [];
    if (p.length < 2) return null;
    const win = rec.result?.window_s ?? 0.01;
    const dur = p.length * win;
    const cols = Math.min(W, p.length);
    const per = p.length / cols;
    const bars: number[] = [];
    for (let c = 0; c < cols; c++) {
      let m = -Infinity;
      for (let k = Math.floor(c * per); k < Math.floor((c + 1) * per); k++) m = Math.max(m, p[k]);
      bars.push(m);
    }
    const lo = Math.min(...bars);
    const hi = Math.max(...bars, lo + 1);
    const y = (d: number) => H - ((d - lo) / (hi - lo)) * (H - 4);
    const x = (s: number) => (s / dur) * W;
    return {
      bars,
      bw: W / cols,
      dur,
      x,
      y,
      thr: rec.result?.threshold_db ?? lo,
      segs: rec.result?.segments ?? [],
    };
  });
  const startUtc = $derived(
    rec.snapshot?.start_ms ? new Date(rec.snapshot.start_ms).toISOString().slice(11, 19) : '',
  );
</script>

<div class="mb-2 rounded border border-sky-900 bg-slate-950/60 px-2 py-1">
  <div class="mb-1 flex items-center justify-between text-[10px] text-[color:var(--color-muted)]">
    <span>
      <span class="text-sky-300">{title}</span>
      <span class="ml-1 opacity-70">· {byLabel} · {rec.snapshot?.id}</span>
    </span>
    <span class="flex items-center gap-2">
      <button
        type="button"
        class="hover:text-slate-200"
        onclick={() => (showText = !showText)}
        title="the text rendering the agent sees">{showText ? 'chart' : 'text'}</button
      >
      <span class="font-mono">{new Date(t).toLocaleTimeString()}</span>
    </span>
  </div>

  {#if showText || (!spec && !env && !(rec.tool === 'spectrogram' && rec.cells_b64))}
    <pre class="overflow-x-auto text-[9px] leading-[10px] text-slate-300">{rec.text ?? ''}</pre>
  {:else if spec}
    <svg viewBox="0 0 {W} {H}" class="h-24 w-full" preserveAspectRatio="none">
      <polyline points={spec.line} fill="none" stroke="#7dd3fc" stroke-width="1" />
      {#each spec.peaks as p (p.freq_hz)}
        <circle
          cx={spec.x(p.freq_hz)}
          cy={spec.y(p.peak_db)}
          r="2.5"
          fill={p.keyed ? '#fbbf24' : '#e2e8f0'}
        />
      {/each}
    </svg>
    <table class="mt-1 w-full text-[10px]">
      <thead class="text-[color:var(--color-muted)]">
        <tr><th class="text-left">MHz</th><th>peak dB</th><th>duty</th><th>width</th><th></th></tr>
      </thead>
      <tbody>
        {#each spec.peaks.slice(0, 8) as p (p.freq_hz)}
          <tr
            class="cursor-pointer hover:bg-slate-800/60"
            onclick={() => tuneTo(p.freq_hz)}
            title="tune here (carrier on the decoder's tone)"
          >
            <td class="font-mono {p.keyed ? 'text-amber-300' : 'text-slate-200'}"
              >{mhz(p.freq_hz)}</td
            >
            <td class="text-center">+{p.peak_db.toFixed(1)}</td>
            <td class="text-center">{p.duty.toFixed(2)}</td>
            <td class="text-center">{p.width_hz.toFixed(0)} Hz</td>
            <td class="text-right text-[9px] text-amber-300">{p.keyed ? 'keyed' : ''}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {:else if rec.tool === 'spectrogram'}
    <canvas bind:this={canvas} class="h-40 w-full" style="image-rendering: pixelated"></canvas>
    <div class="flex justify-between text-[9px] text-[color:var(--color-muted)]">
      <span>{mhz(rec.result?.f0_hz ?? 0)}</span>
      <span>↓ {((rec.result?.dt_s ?? 0) * (rec.result?.rows ?? 0)).toFixed(1)} s</span>
      <span
        >{mhz(
          (rec.result?.f0_hz ?? 0) + (rec.result?.df_hz ?? 0) * ((rec.result?.cols ?? 1) - 1),
        )}</span
      >
    </div>
  {:else if env}
    <svg viewBox="0 0 {W} {H}" class="h-20 w-full" preserveAspectRatio="none">
      {#each env.bars as v, i (i)}
        <rect
          x={i * env.bw}
          y={env.y(v)}
          width={Math.max(0.6, env.bw - 0.2)}
          height={H - env.y(v)}
          fill={v > env.thr ? '#fbbf24' : '#475569'}
        />
      {/each}
      <line
        x1="0"
        x2={W}
        y1={env.y(env.thr)}
        y2={env.y(env.thr)}
        stroke="#94a3b8"
        stroke-dasharray="3 3"
        stroke-width="0.6"
      />
    </svg>
    <div class="flex justify-between text-[9px] text-[color:var(--color-muted)]">
      <span>{startUtc} UTC</span>
      <span
        >duty {(rec.result?.duty ?? 0).toFixed(2)} · swing {(rec.result?.swing_db ?? 0).toFixed(0)} dB{rec
          .result?.keyed
          ? ' · keyed'
          : ''}</span
      >
      <span>+{env.dur.toFixed(1)} s</span>
    </div>
  {/if}
</div>
