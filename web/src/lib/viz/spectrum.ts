// Canvas2D line plot of the most-recent FFT row, plus optional fading
// persistence history, a max-hold trace, and a client-side auto-floor
// that asks the server to re-scale floor/ceil.

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
 *  undefined — the renderer skips that line. */
export interface SpectrumMarkers {
  sdrCenterHz?: number;
  vfoHz?: number;
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

const LEFT_MARGIN = 44;
const BOTTOM_MARGIN = 18;
const TOP_MARGIN = 4;
const RIGHT_MARGIN = 6;

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
        this.maxRow = new Uint8Array(row);
      } else {
        for (let i = 0; i < row.length; i++) {
          if (row[i] > this.maxRow[i]) this.maxRow[i] = row[i];
        }
      }
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

    const floor = this.axes?.floorDbfs ?? 0;
    const ceil = this.axes?.ceilDbfs ?? 255;
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
      ctx.strokeStyle = 'rgba(255, 157, 58, 0.85)';
      ctx.shadowColor = 'rgba(255, 157, 58, 0.55)';
      ctx.shadowBlur = 6 * dpr;
      const x = Math.floor(toX(vfo)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, plot.y);
      ctx.lineTo(x, plot.y + plot.h);
      ctx.stroke();
      ctx.shadowBlur = 0;
    }
  }

  private strokeRow(row: Uint8Array, plot: { x: number; y: number; w: number; h: number }): void {
    if (row.length === 0) return;
    const { ctx } = this;
    const n = row.length;
    ctx.beginPath();
    const xScale = plot.w / (n - 1);
    const yScale = plot.h / 255;
    for (let i = 0; i < n; i++) {
      const x = plot.x + i * xScale;
      const y = plot.y + plot.h - row[i] * yScale;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
}

/** 10th / 99th percentile of a row's byte values. */
export function computeStats(row: Uint8Array): SpectrumStats {
  if (row.length === 0) return { p10: 0, p99: 255 };
  const hist = new Uint32Array(256);
  for (let i = 0; i < row.length; i++) hist[row[i]]++;
  const n = row.length;
  const t10 = Math.max(1, Math.floor(n * 0.1));
  const t99 = Math.max(1, Math.floor(n * 0.99));
  let cum = 0;
  let p10 = 0;
  let p99 = 255;
  let seen10 = false;
  for (let v = 0; v < 256; v++) {
    cum += hist[v];
    if (!seen10 && cum >= t10) {
      p10 = v;
      seen10 = true;
    }
    if (cum >= t99) {
      p99 = v;
      break;
    }
  }
  return { p10, p99 };
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
