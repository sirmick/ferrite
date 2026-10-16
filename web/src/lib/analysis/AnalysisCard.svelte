<script lang="ts">
  // One `analysis` record (server/src/analysis.rs), drawn inline in the AI
  // panel — whoever ran it (the AI over MCP, a person on the CLI, the UI).
  // The record carries structured results; this draws them. `text` (the
  // character rendering the agent sees) is one toggle away.
  import type { AnalysisRecord, Peak } from './types';
  import { decodeCells, heat } from './types';
  // NASA Blue Marble, equirectangular (public domain) — the spot map's
  // basemap; x = lon + 180, y = 90 − lat in its 360 × 180 frame.
  import worldMap from '../../../../tools/assets/bluemarble_2048.jpg';

  // `inline`: drawn inside the tool card of the call that made it, which
  // already says who ran it and when.
  let { rec, t, inline = false }: { rec: AnalysisRecord; t: number; inline?: boolean } = $props();
  let showText = $state(false);

  const mhz = (f: number) => (f / 1e6).toFixed(5);
  const byLabel = $derived(rec.by === 'mcp' ? 'AI' : rec.by === 'cli' ? 'CLI' : 'UI');
  const title = $derived.by(() => {
    const p = rec.params ?? {};
    if (rec.tool === 'envelope') return `envelope @ ${mhz(p.freq_hz ?? 0)} MHz`;
    if (rec.tool === 'inspect') return `inspect @ ${mhz(p.freq_hz ?? 0)} MHz`;
    if (rec.tool === 'sigwiki') {
      const q = (rec.params ?? {}) as Record<string, unknown>;
      if (q['title']) return `sigidwiki · ${q['title']}`;
      return `sigidwiki · what's at ${mhz(Number(q['freq_hz'] ?? 0))} MHz?`;
    }
    if (rec.tool === 'spot_map') {
      const n = rec.result?.spots?.length ?? 0;
      return `${(rec.params as Record<string, unknown> | undefined)?.kind ?? 'ft8'} map · ${n} station${n === 1 ? '' : 's'}`;
    }
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

  // ── spectrogram (and inspect, which carries one) ──────────────────
  const isPicture = $derived(rec.tool === 'spectrogram' || rec.tool === 'inspect');
  const feat = $derived(rec.tool === 'inspect' ? rec.result?.features : null);
  let canvas: HTMLCanvasElement | undefined = $state();
  $effect(() => {
    if (!isPicture || !canvas || !rec.cells_b64) return;
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
  // ── spot map ──────────────────────────────────────────────────────
  interface Spot {
    call: string;
    grid: string;
    lat: number;
    lon: number;
    km?: number;
    bearing?: number;
    snr?: number;
  }
  const map = $derived.by(() => {
    if (rec.tool !== 'spot_map') return null;
    const spots: Spot[] = rec.result?.spots ?? [];
    const rx: { lat: number; lon: number; grid?: string } | null = rec.result?.rx ?? null;
    return { spots, rx };
  });

  // ── sigidwiki ─────────────────────────────────────────────────────
  interface SigCandidate {
    title: string;
    url: string;
    score?: number;
    why?: string[];
    freq_min?: number;
    freq_max?: number;
    bw_min?: number;
    bw_max?: number;
    mode?: string[];
    modulation?: string[];
    description?: string;
    picture?: string;
    location?: string;
  }
  const sig = $derived.by(() => {
    if (rec.tool !== 'sigwiki') return null;
    const r = rec.result ?? {};
    if (r.signal) {
      return {
        page: r.signal as SigCandidate,
        picture: r.signal.picture
          ? `/api/sigwiki/image?file=${encodeURIComponent(r.signal.picture)}&width=640`
          : null,
        audio: (r.audio_url as string | null) ?? null,
        list: [] as SigCandidate[],
        total: 0,
      };
    }
    return {
      page: null,
      picture: null,
      audio: null,
      list: (r.candidates ?? []) as SigCandidate[],
      total: Number(r.total ?? 0),
    };
  });
  const range = (c: SigCandidate) =>
    c.freq_min != null && c.freq_max != null && c.freq_max - c.freq_min > 1
      ? `${(c.freq_min / 1e6).toFixed(3)}–${(c.freq_max / 1e6).toFixed(3)} MHz`
      : c.freq_min != null
        ? `${(c.freq_min / 1e6).toFixed(4)} MHz`
        : '';
  const bw = (c: SigCandidate) => {
    const f = (b: number) => (b >= 1e3 ? `${(b / 1e3).toFixed(1)} kHz` : `${b.toFixed(0)} Hz`);
    if (c.bw_min != null && c.bw_max != null && Math.abs(c.bw_max - c.bw_min) > 0.5)
      return `${f(c.bw_min)}–${f(c.bw_max)}`;
    const b = c.bw_min ?? c.bw_max;
    return b != null ? f(b) : '';
  };

  const startUtc = $derived(
    rec.snapshot?.start_ms ? new Date(rec.snapshot.start_ms).toISOString().slice(11, 19) : '',
  );
</script>

<div class="rounded border border-sky-900 bg-slate-950/60 px-2 py-1" class:mb-2={!inline}>
  <div class="mb-1 flex items-center justify-between text-[10px] text-[color:var(--color-muted)]">
    <span>
      <span class="text-sky-300">{title}</span>
      {#if !inline}<span class="ml-1 opacity-70"
          >· {byLabel}{rec.snapshot?.id ? ` · ${rec.snapshot.id}` : ''}</span
        >{/if}
    </span>
    <span class="flex items-center gap-2">
      <button
        type="button"
        class="hover:text-slate-200"
        onclick={() => (showText = !showText)}
        title="the text rendering the agent sees">{showText ? 'chart' : 'text'}</button
      >
      {#if !inline}<span class="font-mono">{new Date(t).toLocaleTimeString()}</span>{/if}
    </span>
  </div>

  {#if showText || (!spec && !env && !map && !sig && !(isPicture && rec.cells_b64))}
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
  {:else if isPicture}
    {#if feat}
      <div class="mb-1 text-[11px] text-amber-200">{feat.hint}</div>
      <div class="mb-1 font-mono text-[10px] text-slate-400">
        {mhz(feat.freq_hz)} MHz · {Math.round(feat.occupied_hz)} Hz wide · SNR {feat.snr_db.toFixed(
          0,
        )} dB · duty {feat.duty.toFixed(2)}{feat.fsk_shift_hz
          ? ` · shift ${Math.round(feat.fsk_shift_hz)} Hz`
          : ''}{feat.bursts?.period_s ? ` · every ${feat.bursts.period_s.toFixed(2)} s` : ''}
      </div>
    {/if}
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
  {:else if sig?.page}
    {@const p = sig.page}
    <div class="flex gap-2">
      {#if sig.picture}
        <a href={p.url} target="_blank" rel="noopener noreferrer" class="shrink-0">
          <img
            src={sig.picture}
            alt={p.title}
            class="max-h-48 max-w-[45%] rounded border border-slate-700"
          />
        </a>
      {/if}
      <div class="min-w-0 text-[10px] text-slate-300">
        <a
          href={p.url}
          target="_blank"
          rel="noopener noreferrer"
          class="text-[11px] text-sky-300 hover:underline">{p.title}</a
        >
        <div class="text-[color:var(--color-muted)]">
          {range(p)}{bw(p) ? ` · bw ${bw(p)}` : ''} · {(p.mode ?? []).join('/')}
          {(p.modulation ?? []).join('/')}{p.location ? ` · ${p.location}` : ''}
        </div>
        {#if p.description}<div class="mt-1">{p.description}</div>{/if}
        {#if sig.audio}
          <audio controls preload="none" src={sig.audio} class="mt-1 h-7 w-full"></audio>
        {/if}
      </div>
    </div>
  {:else if sig}
    <div class="mb-1 text-[10px] text-[color:var(--color-muted)]">
      {sig.total} signals cover this frequency; best {sig.list.length}
    </div>
    {#each sig.list as c (c.title)}
      <div class="flex items-start gap-2 border-t border-slate-800 py-1 text-[10px]">
        {#if c.picture}
          <img
            src={`/api/sigwiki/image?file=${encodeURIComponent(c.picture)}&width=160`}
            alt=""
            loading="lazy"
            class="h-10 w-14 shrink-0 rounded object-cover"
          />
        {:else}
          <div class="h-10 w-14 shrink-0 rounded bg-slate-900"></div>
        {/if}
        <div class="min-w-0 flex-1">
          <div class="flex justify-between gap-2">
            <a
              href={c.url}
              target="_blank"
              rel="noopener noreferrer"
              class="truncate text-sky-300 hover:underline">{c.title}</a
            >
            <span class="shrink-0 font-mono text-slate-500">{(c.score ?? 0).toFixed(1)}</span>
          </div>
          <div class="truncate text-[color:var(--color-muted)]">
            {range(c)}{bw(c) ? ` · ${bw(c)}` : ''} · {(c.mode ?? []).join('/')}
            {(c.modulation ?? []).join('/')}
          </div>
          {#if c.why?.length}<div class="truncate italic text-slate-500">
              {c.why.join(' · ')}
            </div>{/if}
        </div>
      </div>
    {/each}
  {:else if map}
    <svg viewBox="0 0 360 180" class="w-full rounded" preserveAspectRatio="xMidYMid meet">
      <image href={worldMap} x="0" y="0" width="360" height="180" preserveAspectRatio="none" />
      <clipPath id="map-frame-{rec.id}"><rect x="0" y="0" width="360" height="180" /></clipPath>
      {#if map.rx}
        <g clip-path="url(#map-frame-{rec.id})">
          {#each map.spots as s (s.call)}
            <!-- The short way round: past ±180° the path leaves one edge
               and re-enters the other (the svg clips the overhang). -->
            {@const x1 = map.rx.lon + 180}
            {@const x2 = s.lon + 180}
            {@const wrap = x2 - x1 > 180 ? -360 : x1 - x2 > 180 ? 360 : 0}
            {#each wrap ? [0, 1] : [0] as k (k)}
              <line
                x1={k ? x1 - wrap : x1}
                y1={90 - map.rx.lat}
                x2={k ? x2 : x2 + wrap}
                y2={90 - s.lat}
                stroke="#fbbf24"
                stroke-opacity="0.7"
                stroke-width="0.7"
              />
            {/each}
          {/each}
        </g>
      {/if}
      {#each map.spots as s (s.call)}
        <circle
          cx={s.lon + 180}
          cy={90 - s.lat}
          r="3"
          fill="#fbbf24"
          stroke="#000"
          stroke-width="0.6"
        >
          <title
            >{s.call}
            {s.grid}{s.km ? ` · ${s.km.toFixed(0)} km` : ''}{s.snr != null
              ? ` · ${s.snr} dB`
              : ''}</title
          >
        </circle>
      {/each}
      {#if map.rx}
        <circle
          cx={map.rx.lon + 180}
          cy={90 - map.rx.lat}
          r="3.6"
          fill="#38bdf8"
          stroke="#fff"
          stroke-width="0.8"
        />
      {/if}
    </svg>
    {#if map.spots.length}
      <table class="mt-1 w-full text-[10px]">
        <thead class="text-[color:var(--color-muted)]">
          <tr><th class="text-left">call</th><th>grid</th><th>km</th><th>°</th><th>SNR</th></tr>
        </thead>
        <tbody>
          {#each map.spots.slice(0, 8) as s (s.call)}
            <tr>
              <td class="font-mono text-slate-200">{s.call}</td>
              <td class="text-center font-mono">{s.grid}</td>
              <td class="text-center">{s.km != null ? s.km.toFixed(0) : ''}</td>
              <td class="text-center">{s.bearing != null ? s.bearing.toFixed(0) : ''}</td>
              <td class="text-center">{s.snr != null ? s.snr : ''}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <div class="text-[10px] text-[color:var(--color-muted)]">
        no stations with a grid in the window
      </div>
    {/if}
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
