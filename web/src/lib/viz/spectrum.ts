// Canvas2D line plot of the most-recent FFT row, plus optional fading
// persistence history, a max-hold trace, and a client-side auto-floor
// that asks the server to re-scale floor/ceil.
//
// The CPU math — max-per-column bin collapse, elementwise max-hold,
// p10/p99 row stats — lives in `ferrite-blocks::render` (Rust) and is
// called through the wasm-bindgen bindings in `../wasm/blocks`. That
// module is covered by native `cargo test`; this file stays a thin
// canvas driver. `initFrameDecoder()` (awaited by SessionStore before
// frames flow) initializes the same wasm instance, so the synchronous
// calls below are safe from the first setRow onwards.

import {
  collapse_row_to_columns as wasmCollapseRow,
  compute_spectrum_stats as wasmComputeStats,
  update_max_hold as wasmUpdateMaxHold,
} from '../wasm/blocks/ferrite_blocks';

export interface SpectrumAxes {
  /** Centre RF frequency in Hz — drives the X-axis labels. */
  centerHz: number;
  /** Sample rate in Hz — the X axis spans `centerHz ± rateHz/2`. */
  rateHz: number;
  /** dBFS value mapped to row byte 0 (bottom of the plot). */
  floorDbfs: number;
  /** dBFS value mapped to row byte 255 (top of the plot). */
  ceilDbfs: number;
}

export interface SpectrumOptions {
  color?: string;
  baselineColor?: string;
  gridColor?: string;
  labelColor?: string;
  maxHoldColor?: string;
  /** Number of ghost traces drawn behind the live trace (fade). */
  fadeFrames?: number;
}

/** Vertical markers drawn over the plot. `sdrCenterHz` is the tuner LO
 *  (shown in green, on the right of the pair when VFO < centre);
 *  `vfoHz` is where demodulation is actually pulling a channel out
 *  (shown in orange, on the left when VFO < centre). Either may be
 *  undefined — the renderer skips that line.
 *
 *  `vfoWidthHz` is the post-channelizer bandwidth of the demodulated
 *  channel (i.e. `output_rate_hz` of the Channelizer). When set the
 *  renderer draws a translucent orange band centred on `vfoHz` of
 *  that width, giving the same filter-shape cue as a conventional
 *  SA — you can see what slice of spectrum is being pulled out. */
export interface SpectrumMarkers {
  sdrCenterHz?: number;
  vfoHz?: number;
  vfoWidthHz?: number;
}

/** Client-side display-range override for auto-scale. The server emits
 *  bytes pre-quantized to `[axes.floorDbfs, axes.ceilDbfs]`; when a
 *  display override is set, the renderer remaps those bytes onto a
 *  tighter range for the y-axis and trace height without touching the
 *  server's scaling. No round-trip, no restart — at the cost of keeping
 *  the full 0..255 byte resolution across the narrower display window
 *  (quantization detail may get coarser when the display range is a
 *  small slice of the server range). */
export interface SpectrumDisplayRange {
  floorDbfs: number;
  ceilDbfs: number;
}

export interface SpectrumFeatures {
  /** Keep a fading trail of the last N rows behind the live trace. */
  fade: boolean;
  /** Accumulate a running max across all rows since enabled/reset. */
  maxHold: boolean;
}

/** Diagnostics the auto-floor loop uses; emitted on every setRow. */
export interface SpectrumStats {
  /** 10th percentile of the current row byte values. */
  p10: number;
  /** 99th percentile of the current row byte values. */
  p99: number;
}

/// Inset (in CSS pixels) reserved on the canvas for the dBFS y-axis
/// labels. Exported so the waterfall can match it and the two panes
/// land pixel-aligned along their shared frequency axis.
export const LEFT_MARGIN = 44;
const BOTTOM_MARGIN = 18;
const TOP_MARGIN = 4;
/// Inset (in CSS pixels) reserved on the right for trace breathing
/// room. Exported alongside [`LEFT_MARGIN`] so the waterfall mirrors
/// it.
export const RIGHT_MARGIN = 6;

export class SpectrumRenderer {
  private readonly ctx: CanvasRenderingContext2D;
  private row: Uint8Array | undefined;
  private axes: SpectrumAxes | undefined;
  private rafPending = false;
  private disposed = false;

  // Styling
  private color: string;
  private baselineColor: string;
  private gridColor: string;
  private labelColor: string;
  private maxHoldColor: string;
  private fadeFrames: number;

  // Optional overlays
  private features: SpectrumFeatures = { fade: false, maxHold: false };
  private history: Uint8Array[] = [];
  private maxRow: Uint8Array | undefined;
  private statsListener: ((s: SpectrumStats) => void) | undefined;
  private markers: SpectrumMarkers = {};
  private collapseBuf: Uint8Array | undefined;
  private displayRange: SpectrumDisplayRange | undefined;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    opts: SpectrumOptions = {},
  ) {
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas2d not available');
    this.ctx = ctx;
    this.color = opts.color ?? '#7dd3fc';
    this.baselineColor = opts.baselineColor ?? 'rgba(125, 211, 252, 0.12)';
    this.gridColor = opts.gridColor ?? 'rgba(148, 163, 184, 0.18)';
    this.labelColor = opts.labelColor ?? 'rgba(148, 163, 184, 0.75)';
    this.maxHoldColor = opts.maxHoldColor ?? 'rgba(244, 114, 182, 0.75)';
    this.fadeFrames = opts.fadeFrames ?? 12;
    this.resize();
  }

  setRow(row: Uint8Array): void {
    if (this.disposed) return;
    this.row = row;

    if (this.features.fade) {
      this.history.push(new Uint8Array(row));
      if (this.history.length > this.fadeFrames) {
        this.history.splice(0, this.history.length - this.fadeFrames);
      }
    } else if (this.history.length) {
      this.history = [];
    }

    if (this.features.maxHold) {
      if (!this.maxRow || this.maxRow.length !== row.length) {
        this.maxRow = new Uint8Array(row.length);
      }
      wasmUpdateMaxHold(this.maxRow, row);
    }

    if (this.statsListener) this.statsListener(computeStats(row));
    this.scheduleDraw();
  }

  setAxes(axes: SpectrumAxes | undefined): void {
    if (this.disposed) return;
    this.axes = axes;
    this.scheduleDraw();
  }

  setMarkers(next: SpectrumMarkers): void {
    if (this.disposed) return;
    this.markers = { ...next };
    this.scheduleDraw();
  }

  /** Set a client-side display range. `undefined` clears the override
   *  and the plot reverts to the server's `axes.floor/ceil`. */
  setDisplayRange(next: SpectrumDisplayRange | undefined): void {
    if (this.disposed) return;
    this.displayRange = next ? { ...next } : undefined;
    this.scheduleDraw();
  }

  setFeatures(next: SpectrumFeatures): void {
    const prevFade = this.features.fade;
    const prevMax = this.features.maxHold;
    this.features = { ...next };
    if (prevFade && !next.fade) this.history = [];
    if (prevMax && !next.maxHold) this.maxRow = undefined;
    this.scheduleDraw();
  }

  resetMaxHold(): void {
    this.maxRow = undefined;
    this.scheduleDraw();
  }

  onStats(listener: (s: SpectrumStats) => void): void {
    this.statsListener = listener;
  }

  resize(): void {
    if (this.disposed) return;
    const rect = this.canvas.getBoundingClientRect();
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = Math.max(1, Math.floor(rect.width * dpr));
    const h = Math.max(1, Math.floor(rect.height * dpr));
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    this.scheduleDraw();
  }

  destroy(): void {
    this.disposed = true;
  }

  pixelToFreq(cssX: number): number | undefined {
    if (!this.axes) return undefined;
    const rect = this.canvas.getBoundingClientRect();
    if (rect.width <= 0) return undefined;
    const plotLeft = LEFT_MARGIN;
    const plotW = Math.max(1, rect.width - LEFT_MARGIN - RIGHT_MARGIN);
    const frac = (cssX - plotLeft) / plotW;
    if (!(frac >= 0 && frac <= 1)) return undefined;
    const half = this.axes.rateHz / 2;
    return this.axes.centerHz - half + frac * this.axes.rateHz;
  }

  private scheduleDraw(): void {
    if (this.rafPending || this.disposed) return;
    this.rafPending = true;
    requestAnimationFrame(() => {
      this.rafPending = false;
      this.draw();
    });
  }

  private draw(): void {
    if (this.disposed) return;
    const { ctx } = this;
    const cw = this.canvas.width;
    const ch = this.canvas.height;
    ctx.clearRect(0, 0, cw, ch);

    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const plot = {
      x: LEFT_MARGIN * dpr,
      y: TOP_MARGIN * dpr,
      w: Math.max(1, cw - (LEFT_MARGIN + RIGHT_MARGIN) * dpr),
      h: Math.max(1, ch - (TOP_MARGIN + BOTTOM_MARGIN) * dpr),
    };

    this.drawGrid(plot, dpr);
    this.drawTraces(plot);
  }

  private drawGrid(plot: { x: number; y: number; w: number; h: number }, dpr: number): void {
    const { ctx } = this;
    ctx.save();
    ctx.strokeStyle = this.gridColor;
    ctx.lineWidth = 1;
    ctx.fillStyle = this.labelColor;
    ctx.font = `${10 * dpr}px ui-monospace, SFMono-Regular, Menlo, monospace`;
    ctx.textBaseline = 'middle';

    // Labels follow the *display* range when auto-scale is active, so
    // the dBFS ticks you see match the stretched plot rather than the
    // server's static quantisation window.
    const floor = this.displayRange?.floorDbfs ?? this.axes?.floorDbfs ?? 0;
    const ceil = this.displayRange?.ceilDbfs ?? this.axes?.ceilDbfs ?? 255;
    const yTicks = niceTicks(floor, ceil, 6);
    ctx.textAlign = 'right';
    for (const v of yTicks) {
      const frac = (v - floor) / (ceil - floor);
      if (!(frac >= 0 && frac <= 1)) continue;
      const y = plot.y + plot.h - frac * plot.h;
      ctx.beginPath();
      ctx.moveTo(plot.x, y + 0.5);
      ctx.lineTo(plot.x + plot.w, y + 0.5);
      ctx.stroke();
      ctx.fillText(`${v.toFixed(0)}`, plot.x - 4 * dpr, y);
    }
    if (this.axes) {
      ctx.save();
      ctx.translate(10 * dpr, plot.y + plot.h / 2);
      ctx.rotate(-Math.PI / 2);
      ctx.textAlign = 'center';
      ctx.fillText('dBFS', 0, 0);
      ctx.restore();
    }

    if (this.axes) {
      const half = this.axes.rateHz / 2;
      const fMin = this.axes.centerHz - half;
      const fMax = this.axes.centerHz + half;
      const xTicks = niceTicks(fMin, fMax, 6);
      ctx.textAlign = 'center';
      ctx.textBaseline = 'top';
      for (const f of xTicks) {
        const frac = (f - fMin) / (fMax - fMin);
        if (!(frac >= 0 && frac <= 1)) continue;
        const x = plot.x + frac * plot.w;
        ctx.beginPath();
        ctx.moveTo(x + 0.5, plot.y);
        ctx.lineTo(x + 0.5, plot.y + plot.h);
        ctx.stroke();
        ctx.fillText(fmtMHz(f), x, plot.y + plot.h + 4 * dpr);
      }
    }

    ctx.strokeStyle = this.gridColor;
    ctx.strokeRect(plot.x + 0.5, plot.y + 0.5, plot.w, plot.h);
    ctx.restore();
  }

  private drawTraces(plot: { x: number; y: number; w: number; h: number }): void {
    const { ctx } = this;
    ctx.save();
    ctx.beginPath();
    ctx.rect(plot.x, plot.y, plot.w, plot.h);
    ctx.clip();

    ctx.strokeStyle = this.baselineColor;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(plot.x, plot.y + plot.h - 0.5);
    ctx.lineTo(plot.x + plot.w, plot.y + plot.h - 0.5);
    ctx.stroke();

    this.drawMarkers(plot);

    // Fade trail: older → dimmer.
    if (this.features.fade && this.history.length > 1) {
      const n = this.history.length;
      for (let i = 0; i < n - 1; i++) {
        const age = (n - 1 - i) / n;
        ctx.strokeStyle = this.color;
        ctx.globalAlpha = Math.max(0.05, 0.35 * (1 - age));
        this.strokeRow(this.history[i], plot);
      }
      ctx.globalAlpha = 1;
    }

    // Max hold trace.
    if (this.features.maxHold && this.maxRow) {
      ctx.strokeStyle = this.maxHoldColor;
      ctx.lineWidth = Math.max(1, Math.floor(Math.min(window.devicePixelRatio || 1, 2)));
      this.strokeRow(this.maxRow, plot);
    }

    // Live trace last (on top).
    if (this.row) {
      ctx.strokeStyle = this.color;
      ctx.lineWidth = Math.max(1, Math.floor(Math.min(window.devicePixelRatio || 1, 2)));
      this.strokeRow(this.row, plot);
    }
    ctx.restore();
  }

  private drawMarkers(plot: { x: number; y: number; w: number; h: number }): void {
    if (!this.axes) return;
    const { ctx } = this;
    const half = this.axes.rateHz / 2;
    const fMin = this.axes.centerHz - half;
    const fMax = this.axes.centerHz + half;
    const toX = (hz: number) => plot.x + ((hz - fMin) / (fMax - fMin)) * plot.w;
    const clamp = (hz: number) => hz >= fMin && hz <= fMax;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);

    ctx.lineWidth = Math.max(1, Math.floor(dpr));

    const sdr = this.markers.sdrCenterHz;
    if (sdr !== undefined && clamp(sdr)) {
      ctx.strokeStyle = 'rgba(88, 240, 160, 0.75)';
      ctx.shadowColor = 'rgba(88, 240, 160, 0.45)';
      ctx.shadowBlur = 6 * dpr;
      const x = Math.floor(toX(sdr)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, plot.y);
      ctx.lineTo(x, plot.y + plot.h);
      ctx.stroke();
      ctx.shadowBlur = 0;
    }

    const vfo = this.markers.vfoHz;
    if (vfo !== undefined && clamp(vfo)) {
      // Draw the channel-width band first so the VFO line sits on top.
      // Band is clipped to the plot — half-channels outside the view
      // still render the visible slice rather than disappearing.
      const width = this.markers.vfoWidthHz;
      if (width !== undefined && width > 0) {
        const bandLo = Math.max(fMin, vfo - width / 2);
        const bandHi = Math.min(fMax, vfo + width / 2);
        if (bandHi > bandLo) {
          const xLo = toX(bandLo);
          const xHi = toX(bandHi);
          ctx.fillStyle = 'rgba(255, 157, 58, 0.12)';
          ctx.fillRect(xLo, plot.y, xHi - xLo, plot.h);
          // Subtle edge lines at the band boundaries — easier to read
          // the exact channel edges than relying on a washed-out fill.
          ctx.strokeStyle = 'rgba(255, 157, 58, 0.4)';
          ctx.lineWidth = Math.max(1, Math.floor(dpr));
          ctx.beginPath();
          const xLoLine = Math.floor(xLo) + 0.5;
          const xHiLine = Math.floor(xHi) + 0.5;
          ctx.moveTo(xLoLine, plot.y);
          ctx.lineTo(xLoLine, plot.y + plot.h);
          ctx.moveTo(xHiLine, plot.y);
          ctx.lineTo(xHiLine, plot.y + plot.h);
          ctx.stroke();
        }
      }

      ctx.strokeStyle = 'rgba(255, 157, 58, 0.85)';
      ctx.shadowColor = 'rgba(255, 157, 58, 0.55)';
      ctx.shadowBlur = 6 * dpr;
      ctx.lineWidth = Math.max(1, Math.floor(dpr));
      const x = Math.floor(toX(vfo)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, plot.y);
      ctx.lineTo(x, plot.y + plot.h);
      ctx.stroke();
      ctx.shadowBlur = 0;
    }
  }

  /** Lookup table mapping byte `b` → pixel y, factoring in the display
   *  override. Cached across draws with the same plot height + axes +
   *  override. Avoids per-sample `log`/`mul` in the hot loop — the
   *  16384-bin FFT at 30 Hz is otherwise where this function spends a
   *  measurable chunk of frame time. */
  private byteToYCache: { key: string; lut: Float32Array } | undefined;
  private byteToYLut(plot: { y: number; h: number }): Float32Array {
    const serverFloor = this.axes?.floorDbfs ?? 0;
    const serverCeil = this.axes?.ceilDbfs ?? 255;
    const displayFloor = this.displayRange?.floorDbfs ?? serverFloor;
    const displayCeil = this.displayRange?.ceilDbfs ?? serverCeil;
    const key = `${plot.y}_${plot.h}_${serverFloor}_${serverCeil}_${displayFloor}_${displayCeil}`;
    if (this.byteToYCache && this.byteToYCache.key === key) return this.byteToYCache.lut;
    const lut = new Float32Array(256);
    const serverRange = serverCeil - serverFloor;
    const displayRange = displayCeil - displayFloor;
    for (let b = 0; b < 256; b++) {
      if (this.displayRange && displayRange > 0 && serverRange > 0) {
        const db = serverFloor + (b / 255) * serverRange;
        const frac = Math.max(0, Math.min(1, (db - displayFloor) / displayRange));
        lut[b] = plot.y + plot.h - frac * plot.h;
      } else {
        lut[b] = plot.y + plot.h - (b / 255) * plot.h;
      }
    }
    this.byteToYCache = { key, lut };
    return lut;
  }

  private strokeRow(row: Uint8Array, plot: { x: number; y: number; w: number; h: number }): void {
    if (row.length === 0) return;
    const { ctx } = this;
    const n = row.length;
    const lut = this.byteToYLut(plot);
    ctx.beginPath();

    // When the FFT has more bins than pixels (e.g. 16384 bins across a
    // ~1200 px plot), drawing a lineTo per bin paints multiple bins into
    // the same column — adjacent noisy bins jump in y and the canvas
    // renders that as visible vertical strokes (the "comb"). Collapse
    // each pixel column to its max bin so the trace is a clean envelope.
    // The reduction runs in wasm (`ferrite-blocks::render`).
    const px = Math.max(1, Math.floor(plot.w));
    if (n > px) {
      if (!this.collapseBuf || this.collapseBuf.length !== px) {
        this.collapseBuf = new Uint8Array(px);
      }
      wasmCollapseRow(row, this.collapseBuf);
      const cols = this.collapseBuf;
      for (let i = 0; i < px; i++) {
        const x = plot.x + i;
        const y = lut[cols[i]!]!;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
    } else {
      const xScale = plot.w / (n - 1);
      for (let i = 0; i < n; i++) {
        const x = plot.x + i * xScale;
        const y = lut[row[i]!]!;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
    }
    ctx.stroke();
  }
}

/** 10th / 99th percentile of a row's byte values. Thin wrapper over
 *  the wasm `compute_spectrum_stats`; frees the Rust-owned handle
 *  before returning a plain-object snapshot. */
export function computeStats(row: Uint8Array): SpectrumStats {
  const s = wasmComputeStats(row);
  try {
    return { p10: s.p10, p99: s.p99 };
  } finally {
    s.free();
  }
}

function niceTicks(lo: number, hi: number, target: number): number[] {
  if (!(hi > lo) || !isFinite(lo) || !isFinite(hi) || target < 2) return [];
  const range = niceNum(hi - lo, false);
  const step = niceNum(range / (target - 1), true);
  const start = Math.ceil(lo / step) * step;
  const out: number[] = [];
  for (let v = start; v <= hi + step * 1e-6; v += step) {
    out.push(Number(v.toFixed(10)));
  }
  return out;
}

function niceNum(x: number, round: boolean): number {
  if (x === 0) return 0;
  const exp = Math.floor(Math.log10(Math.abs(x)));
  const f = Math.abs(x) / Math.pow(10, exp);
  let nf: number;
  if (round) {
    if (f < 1.5) nf = 1;
    else if (f < 3) nf = 2;
    else if (f < 7) nf = 5;
    else nf = 10;
  } else {
    if (f <= 1) nf = 1;
    else if (f <= 2) nf = 2;
    else if (f <= 5) nf = 5;
    else nf = 10;
  }
  return Math.sign(x) * nf * Math.pow(10, exp);
}

function fmtMHz(hz: number): string {
  const mhz = hz / 1e6;
  const abs = Math.abs(mhz);
  if (abs >= 100) return `${mhz.toFixed(2)}`;
  if (abs >= 1) return `${mhz.toFixed(3)}`;
  return `${mhz.toFixed(4)}`;
}
