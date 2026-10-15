<script lang="ts">
  // The spectrum analyzer's one content area: the stitched trace on top,
  // a stitched waterfall below (one line per pass, filled step by step as
  // rows arrive). Canvas 2D — at ≤ ~11 rows/s it doesn't need WebGL.
  //
  // Gestures (mirroring the receiver's grammar):
  //   click        → marker (readout in the corner)
  //   drag         → sweep the selected range
  //   double-click → listen there (back to the receiver, tuned)
  //   right-click  → zoom out ×2
  import { onMount } from 'svelte';
  import Split from '$lib/layout/Split.svelte';
  import { makeSigidwikiLut } from '$lib/viz/colormap';
  import { registerView, unregisterView } from '$lib/viz/viewRegistry';
  import { pipeline } from '$lib/pipeline.svelte';
  import { specan } from './specan.svelte';
  import {
    autoRange,
    fmtHz,
    reduceToColumns,
    RowCache,
    ticks,
    TraceHolds,
    type DecodedRow,
  } from './panorama';

  const GUTTER = 44; // dB axis width, px
  const AXIS = 18; // freq axis height, px
  const LUT = makeSigidwikiLut(256);

  let traceCanvas: HTMLCanvasElement | undefined = $state();
  let wfCanvas: HTMLCanvasElement | undefined = $state();
  let traceBox = $state({ w: 0, h: 0 });
  let wfBox = $state({ w: 0, h: 0 });

  const cache = new RowCache();
  const holds = new TraceHolds();
  let decoded = $derived<DecodedRow[]>(cache.decode(specan.rows));
  let lo = $derived(specan.range[0]);
  let hi = $derived(specan.range[1]);
  let plotW = $derived(Math.max(1, Math.floor(traceBox.w - GUTTER)));
  let live = $derived(reduceToColumns(decoded, lo, hi, plotW));
  let newest = $derived(decoded.reduce((m, r) => Math.max(m, r.t_ms), 0));

  let dbRange = $state<[number, number]>([-110, -30]);
  let hover = $state<{ x: number; f: number; db: number } | null>(null);
  let drag = $state<{ x0: number; x1: number } | null>(null);

  const freqAt = (x: number) => lo + ((x - GUTTER) / plotW) * (hi - lo);
  const xAt = (f: number) => GUTTER + ((f - lo) / (hi - lo)) * plotW;

  function sizeCanvas(c: HTMLCanvasElement, w: number, h: number) {
    const dpr = window.devicePixelRatio || 1;
    if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
      c.width = Math.round(w * dpr);
      c.height = Math.round(h * dpr);
    }
    const ctx = c.getContext('2d')!;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return ctx;
  }

  // ── trace ────────────────────────────────────────────────────────────
  $effect(() => {
    void specan.clearToken;
    holds.clear();
  });

  $effect(() => {
    const c = traceCanvas;
    const { w, h } = traceBox;
    if (!c || w < 10 || h < 10) return;
    const cols = live;
    holds.update(cols, `${lo}|${hi}|${plotW}|${specan.clearToken}`, newest);
    const trace = holds.pick(specan.traceMode, cols);
    // Auto-scale follows the live trace, so switching to a hold doesn't
    // shift the scale (or the waterfall's colours with it).
    const [d0, d1] = specan.autoDb ? autoRange(cols) : [specan.dbLo, specan.dbHi];
    dbRange = [d0, d1];
    const ctx = sizeCanvas(c, w, h);
    const plotH = h - AXIS;
    const yAt = (db: number) => ((d1 - db) / (d1 - d0)) * plotH;
    ctx.fillStyle = '#020617';
    ctx.fillRect(0, 0, w, h);
    // grid + dB labels
    ctx.font = '10px ui-monospace, monospace';
    ctx.strokeStyle = '#1e293b';
    ctx.fillStyle = '#64748b';
    ctx.lineWidth = 1;
    for (const db of ticks(d0, d1, 6)) {
      const y = Math.round(yAt(db)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(GUTTER, y);
      ctx.lineTo(w, y);
      ctx.stroke();
      ctx.fillText(`${db}`, 4, y + 3);
    }
    for (const f of ticks(lo, hi, Math.max(3, Math.floor(plotW / 110)))) {
      const x = Math.round(xAt(f)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, plotH);
      ctx.stroke();
      ctx.fillText(fmtHz(f, hi - lo), x + 3, h - 5);
    }
    ctx.fillText('dBFS', 4, plotH - 4);
    // live under a hold, faint
    const path = (arr: Float32Array, color: string, width: number) => {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.beginPath();
      let pen = false;
      for (let i = 0; i < arr.length; i++) {
        const v = arr[i];
        if (Number.isNaN(v)) {
          pen = false;
          continue;
        }
        const x = GUTTER + i + 0.5;
        const y = yAt(Math.max(d0, Math.min(d1, v)));
        if (pen) ctx.lineTo(x, y);
        else ctx.moveTo(x, y);
        pen = true;
      }
      ctx.stroke();
    };
    if (specan.traceMode !== 'live') path(cols, 'rgba(56,189,248,0.25)', 1);
    const colour = { live: '#38bdf8', max: '#f59e0b', min: '#a78bfa', avg: '#34d399' }[
      specan.traceMode
    ];
    path(trace, colour, 1.25);
    // marker
    if (specan.marker !== null && specan.marker >= lo && specan.marker <= hi) {
      const x = Math.round(xAt(specan.marker)) + 0.5;
      const v = trace[Math.min(trace.length - 1, Math.max(0, Math.round(x - GUTTER)))];
      ctx.strokeStyle = '#fb923c';
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, plotH);
      ctx.stroke();
      ctx.fillStyle = '#fb923c';
      ctx.fillText(
        `M ${fmtHz(specan.marker, 1)}  ${Number.isNaN(v) ? '—' : v.toFixed(1)} dBFS`,
        GUTTER + 6,
        12,
      );
    }
    // drag selection
    if (drag) {
      const a = Math.min(drag.x0, drag.x1);
      const b = Math.max(drag.x0, drag.x1);
      ctx.fillStyle = 'rgba(125,211,252,0.12)';
      ctx.fillRect(a, 0, b - a, plotH);
    }
    // hover readout
    if (hover) {
      ctx.strokeStyle = 'rgba(148,163,184,0.5)';
      ctx.beginPath();
      ctx.moveTo(Math.round(hover.x) + 0.5, 0);
      ctx.lineTo(Math.round(hover.x) + 0.5, plotH);
      ctx.stroke();
      ctx.fillStyle = '#cbd5e1';
      const txt = `${fmtHz(hover.f, 1)}  ${Number.isNaN(hover.db) ? '—' : hover.db.toFixed(1)} dBFS`;
      ctx.fillText(txt, Math.min(w - 170, hover.x + 6), 26);
    }
  });

  // ── waterfall ────────────────────────────────────────────────────────
  // An image buffer one line per pass. A row paints its columns into the
  // top line; a new pass number scrolls everything down first.
  let wfImg: ImageData | null = null;
  let wfKey = '';
  let wfPass = -1;
  const painted = new Map<number, number>(); // step → t_ms painted

  $effect(() => {
    const c = wfCanvas;
    const { w, h } = wfBox;
    if (!c || w < 10 || h < 10) return;
    const W = Math.max(1, Math.floor(w - GUTTER));
    const H = Math.max(1, Math.floor(h));
    const key = `${lo}|${hi}|${W}|${H}|${decoded[0]?.cfg ?? 0}`;
    if (key !== wfKey || !wfImg) {
      wfImg = new ImageData(W, H);
      wfKey = key;
      wfPass = -1;
      painted.clear();
    }
    const img = wfImg;
    const [d0, d1] = dbRange;
    for (const r of decoded) {
      if (painted.get(r.step) === r.t_ms) continue;
      painted.set(r.step, r.t_ms);
      // Only a *newer* pass scrolls (a snapshot can mix the last two).
      if (r.pass > wfPass) {
        if (wfPass !== -1) img.data.copyWithin(W * 4, 0, W * 4 * (H - 1));
        img.data.fill(0, 0, W * 4);
        wfPass = r.pass;
      }
      const cols = reduceToColumns([r], lo, hi, W);
      for (let x = 0; x < W; x++) {
        const v = cols[x];
        if (Number.isNaN(v)) continue;
        const t = Math.max(0, Math.min(255, Math.round(((v - d0) / (d1 - d0)) * 255)));
        const o = x * 4;
        img.data[o] = LUT[t * 4];
        img.data[o + 1] = LUT[t * 4 + 1];
        img.data[o + 2] = LUT[t * 4 + 2];
        img.data[o + 3] = 255;
      }
    }
    const ctx = sizeCanvas(c, w, h);
    ctx.fillStyle = '#020617';
    ctx.fillRect(0, 0, w, h);
    // draw at device scale 1:1 in CSS px
    const tmp = new OffscreenCanvas(W, H);
    tmp.getContext('2d')!.putImageData(img, 0, 0);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(tmp, GUTTER, 0, W, H);
    if (specan.marker !== null && specan.marker >= lo && specan.marker <= hi) {
      ctx.strokeStyle = 'rgba(251,146,60,0.7)';
      const x = Math.round(xAt(specan.marker)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
    }
    ctx.fillStyle = '#64748b';
    ctx.font = '10px ui-monospace, monospace';
    ctx.fillText('pass', 4, 12);
    ctx.fillText('↓', 4, 24);
  });

  // ── gestures ─────────────────────────────────────────────────────────
  function localX(e: MouseEvent): number {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return e.clientX - r.left;
  }

  function onMove(e: MouseEvent) {
    const x = localX(e);
    if (x < GUTTER) {
      hover = null;
      return;
    }
    if (drag) drag = { ...drag, x1: x };
    const i = Math.min(live.length - 1, Math.max(0, Math.round(x - GUTTER)));
    hover = { x, f: freqAt(x), db: holds.pick(specan.traceMode, live)[i] };
  }

  function onDown(e: MouseEvent) {
    if (e.button !== 0) return;
    const x = localX(e);
    if (x < GUTTER) return;
    drag = { x0: x, x1: x };
  }

  function onUp(e: MouseEvent) {
    if (!drag) return;
    const { x0, x1 } = drag;
    drag = null;
    if (Math.abs(x1 - x0) > 6) {
      void specan.setRange(freqAt(Math.min(x0, x1)), freqAt(Math.max(x0, x1)));
    } else if (e.detail === 1) {
      specan.marker = freqAt(x0);
    }
  }

  async function onDbl(e: MouseEvent) {
    const x = localX(e);
    if (x < GUTTER) return;
    const f = freqAt(x);
    // Leave the analyzer (the server restores the receiver preset), then
    // tune there through the normal dodge-aware path.
    await pipeline.returnPreset();
    await pipeline.tune(f);
  }

  function onContext(e: MouseEvent) {
    e.preventDefault();
    void specan.zoomOut();
  }

  // Screenshot button / `ferrite-ctl view` read these pane names.
  onMount(() => {
    const shotTrace = () => traceCanvas!.toDataURL('image/png').split(',')[1] ?? '';
    const shotWf = () => wfCanvas!.toDataURL('image/png').split(',')[1] ?? '';
    registerView('wide-spectrum', shotTrace);
    registerView('wide-waterfall', shotWf);
    return () => {
      unregisterView('wide-spectrum', shotTrace);
      unregisterView('wide-waterfall', shotWf);
    };
  });
</script>

<div class="flex h-full w-full min-h-0 flex-col">
  <Split direction="column" defaultFraction={0.45} storageKey="ferrite.split.panorama">
    {#snippet a()}
      <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
        <header class="panel-head flex items-center gap-3">
          <span>Spectrum</span>
          <span class="font-mono text-[10px] text-slate-500">
            {fmtHz(lo, hi - lo)} – {fmtHz(hi, hi - lo)}
            {#if decoded[0]}· RBW {decoded[0].rbw_hz.toFixed(0)} Hz · {decoded[0].steps} steps{/if}
          </span>
          {#if decoded.length === 0}
            <span class="text-[10px] text-amber-400">waiting for the first sweep…</span>
          {/if}
        </header>
        <div
          class="relative min-h-0 flex-1 cursor-crosshair select-none"
          bind:clientWidth={traceBox.w}
          bind:clientHeight={traceBox.h}
          onmousemove={onMove}
          onmouseleave={() => {
            hover = null;
            drag = null;
          }}
          onmousedown={onDown}
          onmouseup={onUp}
          ondblclick={onDbl}
          oncontextmenu={onContext}
          role="presentation"
        >
          <canvas
            bind:this={traceCanvas}
            class="absolute inset-0 h-full w-full"
            aria-label="Panorama spectrum trace"
          ></canvas>
        </div>
      </section>
    {/snippet}
    {#snippet b()}
      <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
        <header class="panel-head"><span>Waterfall</span></header>
        <div class="relative min-h-0 flex-1" bind:clientWidth={wfBox.w} bind:clientHeight={wfBox.h}>
          <canvas
            bind:this={wfCanvas}
            class="absolute inset-0 h-full w-full"
            aria-label="Panorama waterfall"
          ></canvas>
        </div>
      </section>
    {/snippet}
  </Split>
</div>
