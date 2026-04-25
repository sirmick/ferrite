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
import { bandsInRange, type Band } from '../presets/bandplan';

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
  /** Sub-grid lines drawn between the major ticks. Should be much
   *  fainter than `gridColor` so the major grid still reads clearly. */
  minorGridColor?: string;
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

/** Display-only horizontal view window. Crops the rendered FFT to a
 *  sub-range of the full sample-rate span without touching the source
 *  or the server. `centerHz` and `rateHz` are clamped at draw time so
 *  the view never extends past the available span. Pass `undefined`
 *  to render the full span (the default). */
export interface SpectrumView {
  centerHz: number;
  rateHz: number;
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
/// Vertical strip (CSS pixels) reserved above the trace for the band-
/// plan ribbon when one is set. Sized for [`BAND_LABEL_ROWS`] stacked
/// rows of ~10 px text — labels stagger across rows so adjacent bands
/// never crowd each other, mirroring the NTIA chart layout. Eats trace
/// headroom but the trade is worth it for the "what am I looking at"
/// cue.
const BAND_STRIP_CSS = 36;
/// Number of stacked label rows in the band ribbon. Each band still
/// paints the full strip height; only the *labels* are distributed
/// across rows by a left-to-right greedy first-fit so neighbours don't
/// collide. Three rows handles every realistic SDR span without
/// dropping more than a handful of fully-buried bands.
const BAND_LABEL_ROWS = 3;

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
  private minorGridColor: string;
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
  private bandPlan: ReadonlyArray<Band> | undefined;
  private view: SpectrumView | undefined;

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
    this.minorGridColor = opts.minorGridColor ?? 'rgba(148, 163, 184, 0.06)';
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

  /** Set the frequency-allocation band plan rendered as a label strip
   *  above the trace. Pass `undefined` to hide the strip and return the
   *  full plot height to the trace. */
  setBandPlan(bands: ReadonlyArray<Band> | undefined): void {
    if (this.disposed) return;
    this.bandPlan = bands;
    this.scheduleDraw();
  }

  /** Crop the rendered FFT to a horizontal sub-range of the full span.
   *  Pass `undefined` to revert to full-span. The view is clamped at
   *  draw time so callers can hand in any plausible window without
   *  pre-validating the edges. */
  setView(view: SpectrumView | undefined): void {
    if (this.disposed) return;
    this.view = view ? { ...view } : undefined;
    this.byteToYCache = undefined;
    this.scheduleDraw();
  }

  /** The clamped (centerHz, rateHz) window currently being rendered.
   *  Falls back to the full axes span when no view is set or the axes
   *  haven't been received yet. Returns `undefined` only when there's
   *  nothing to render. Used by every freq → x mapping in the file so
   *  zoom/pan stays consistent across grid, trace, markers, band strip
   *  and `pixelToFreq`. */
  private renderWindow(): { centerHz: number; rateHz: number } | undefined {
    if (!this.axes) return undefined;
    const v = this.view;
    if (!v) return { centerHz: this.axes.centerHz, rateHz: this.axes.rateHz };
    const halfFull = this.axes.rateHz / 2;
    const halfView = Math.min(halfFull, Math.max(1, v.rateHz) / 2);
    const minC = this.axes.centerHz - (halfFull - halfView);
    const maxC = this.axes.centerHz + (halfFull - halfView);
    const c = Math.max(minC, Math.min(maxC, v.centerHz));
    return { centerHz: c, rateHz: halfView * 2 };
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
    const w = this.renderWindow();
    if (!w) return undefined;
    const rect = this.canvas.getBoundingClientRect();
    if (rect.width <= 0) return undefined;
    const plotLeft = LEFT_MARGIN;
    const plotW = Math.max(1, rect.width - LEFT_MARGIN - RIGHT_MARGIN);
    const frac = (cssX - plotLeft) / plotW;
    if (!(frac >= 0 && frac <= 1)) return undefined;
    return w.centerHz - w.rateHz / 2 + frac * w.rateHz;
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
    // The band strip eats from the top of the plot only when there's a
    // plan to render and an axis to project against; absent either, the
    // trace gets the full height back.
    const stripH = this.bandPlan && this.axes ? BAND_STRIP_CSS * dpr : 0;
    const plot = {
      x: LEFT_MARGIN * dpr,
      y: TOP_MARGIN * dpr + stripH,
      w: Math.max(1, cw - (LEFT_MARGIN + RIGHT_MARGIN) * dpr),
      h: Math.max(1, ch - (TOP_MARGIN + BOTTOM_MARGIN) * dpr - stripH),
    };

    this.drawGrid(plot, dpr);
    this.drawTraces(plot);
    if (stripH > 0) this.drawBandStrip(plot, dpr, stripH);
  }

  private drawGrid(plot: { x: number; y: number; w: number; h: number }, dpr: number): void {
    const { ctx } = this;
    ctx.save();
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

    // Faint sub-grid drawn first so the major lines sit on top.
    // `MINOR_PER_MAJOR` of 5 gives 4 minor ticks between every major;
    // collisions with majors are skipped so the major intensity stays
    // distinct.
    const MINOR_PER_MAJOR = 5;
    if (yTicks.length >= 2) {
      const step = (yTicks[1] as number) - (yTicks[0] as number);
      const minor = step / MINOR_PER_MAJOR;
      ctx.strokeStyle = this.minorGridColor;
      const start = Math.ceil(floor / minor) * minor;
      for (let v = start; v <= ceil; v += minor) {
        if (yTicks.some((mt) => Math.abs(v - mt) < minor / 2)) continue;
        const frac = (v - floor) / (ceil - floor);
        if (!(frac >= 0 && frac <= 1)) continue;
        const y = plot.y + plot.h - frac * plot.h;
        ctx.beginPath();
        ctx.moveTo(plot.x, y + 0.5);
        ctx.lineTo(plot.x + plot.w, y + 0.5);
        ctx.stroke();
      }
    }

    ctx.strokeStyle = this.gridColor;
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

    const win = this.renderWindow();
    if (win) {
      const half = win.rateHz / 2;
      const fMin = win.centerHz - half;
      const fMax = win.centerHz + half;
      const xTicks = niceTicks(fMin, fMax, 6);

      if (xTicks.length >= 2) {
        const step = (xTicks[1] as number) - (xTicks[0] as number);
        const minor = step / MINOR_PER_MAJOR;
        ctx.strokeStyle = this.minorGridColor;
        const start = Math.ceil(fMin / minor) * minor;
        for (let f = start; f <= fMax; f += minor) {
          if (xTicks.some((mt) => Math.abs(f - mt) < minor / 2)) continue;
          const frac = (f - fMin) / (fMax - fMin);
          if (!(frac >= 0 && frac <= 1)) continue;
          const x = plot.x + frac * plot.w;
          ctx.beginPath();
          ctx.moveTo(x + 0.5, plot.y);
          ctx.lineTo(x + 0.5, plot.y + plot.h);
          ctx.stroke();
        }
      }

      ctx.strokeStyle = this.gridColor;
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
    const win = this.renderWindow();
    if (!win) return;
    const { ctx } = this;
    const half = win.rateHz / 2;
    const fMin = win.centerHz - half;
    const fMax = win.centerHz + half;
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

  /** Coloured ribbon above the trace naming each visible band. Bands
   *  come from the static USA plan (`presets/bandplan.ts`); we filter to
   *  whatever overlaps the current axes window and lay them out using
   *  the same linear `freq → x` projection as the grid, so labels line
   *  up exactly with the FFT bins they describe.
   *
   *  Layout has two passes: the colour fills paint full-strip-height per
   *  band so the ribbon is continuous; the labels are then placed by a
   *  greedy left-to-right first-fit across [`BAND_LABEL_ROWS`] rows so
   *  adjacent narrow bands stagger vertically instead of crowding into
   *  one line. Bands whose label can't fit in any row at this zoom drop
   *  the label and stay a clean colour tile. */
  private drawBandStrip(
    plot: { x: number; y: number; w: number; h: number },
    dpr: number,
    stripH: number,
  ): void {
    if (!this.bandPlan) return;
    const win = this.renderWindow();
    if (!win) return;
    const { ctx } = this;
    const half = win.rateHz / 2;
    const fMin = win.centerHz - half;
    const fMax = win.centerHz + half;
    if (!(fMax > fMin)) return;
    const stripY = plot.y - stripH;
    const rowH = stripH / BAND_LABEL_ROWS;

    ctx.save();
    ctx.beginPath();
    ctx.rect(plot.x, stripY, plot.w, stripH);
    ctx.clip();

    // Backing for unallocated gaps so the ribbon still reads as one
    // continuous band even where the plan has holes (e.g. between
    // adjacent VLF carriers).
    ctx.fillStyle = 'rgba(15, 23, 42, 0.6)';
    ctx.fillRect(plot.x, stripY, plot.w, stripH);

    // Pass 1 — colour fills, full strip height per band.
    const visible = bandsInRange(fMin, fMax);
    const slots: { x0: number; x1: number; w: number; band: Band }[] = [];
    for (const band of visible) {
      const lo = Math.max(fMin, band.startHz);
      const hi = Math.min(fMax, band.endHz);
      const x0 = plot.x + ((lo - fMin) / (fMax - fMin)) * plot.w;
      const x1 = plot.x + ((hi - fMin) / (fMax - fMin)) * plot.w;
      const w = x1 - x0;
      if (w < 0.5) continue;
      const [r, g, b] = band.rgb;
      ctx.fillStyle = `rgba(${r}, ${g}, ${b}, 0.6)`;
      ctx.fillRect(x0, stripY, w, stripH);
      slots.push({ x0, x1, w, band });
    }

    // Faint horizontal separators between rows so the staggered layout
    // reads as deliberate rows rather than scattered text.
    ctx.strokeStyle = 'rgba(15, 23, 42, 0.25)';
    ctx.lineWidth = 1;
    for (let r = 1; r < BAND_LABEL_ROWS; r++) {
      const y = Math.floor(stripY + rowH * r) + 0.5;
      ctx.beginPath();
      ctx.moveTo(plot.x, y);
      ctx.lineTo(plot.x + plot.w, y);
      ctx.stroke();
    }

    // Pass 2 — labels. Greedy first-fit across [`BAND_LABEL_ROWS`] rows:
    // try the full band name first (it's allowed to overflow the band's
    // own slot); each row tracks the right edge of its last label so a
    // colliding label bumps to the next row. Only when *no* row has
    // room do we fall back to a truncated tag that fits within the
    // band's own width — that way wide labels stagger across rows
    // (mirroring the NTIA chart) instead of all collapsing into row 0
    // because pre-trimmed labels never overlap their neighbours' centres.
    const fontPx = Math.round(10 * dpr);
    ctx.font = `${fontPx}px ui-sans-serif, system-ui, -apple-system, sans-serif`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    const padPx = 3 * dpr;
    const gapPx = 6 * dpr;
    const lastEndX: number[] = new Array(BAND_LABEL_ROWS).fill(-Infinity);
    for (const slot of slots) {
      const cx = slot.x0 + slot.w / 2;
      const fullText = slot.band.name;
      const fullW = ctx.measureText(fullText).width;

      // First pass: try the full label, allowed to overflow the slot.
      let chosenText = fullText;
      let chosenW = fullW;
      let row = -1;
      for (let r = 0; r < BAND_LABEL_ROWS; r++) {
        if (cx - chosenW / 2 >= lastEndX[r] + gapPx) {
          row = r;
          break;
        }
      }

      // No row had room for the full label — try a slot-bounded truncation
      // (still useful for narrow bands sandwiched between wider ones).
      if (row < 0) {
        const maxTextW = Math.max(0, slot.w - 2 * padPx);
        if (maxTextW <= 0) continue;
        const trunc = ellipsize(ctx, fullText, maxTextW);
        if (!trunc) continue;
        chosenText = trunc;
        chosenW = ctx.measureText(trunc).width;
        for (let r = 0; r < BAND_LABEL_ROWS; r++) {
          if (cx - chosenW / 2 >= lastEndX[r] + gapPx) {
            row = r;
            break;
          }
        }
        if (row < 0) continue;
      }

      lastEndX[row] = cx + chosenW / 2;
      const cy = stripY + rowH * row + rowH / 2 + 0.5;
      // White text with a soft dark drop-shadow reads cleanly against
      // both the pastel and the saturated bands in the SDR++ palette,
      // without per-label backplates that would break the ribbon look.
      ctx.shadowColor = 'rgba(0, 0, 0, 0.75)';
      ctx.shadowBlur = 2 * dpr;
      ctx.fillStyle = 'rgba(245, 245, 245, 0.95)';
      ctx.fillText(chosenText, cx, cy);
      ctx.shadowBlur = 0;
    }

    // Hairline beneath the strip — visually peels it away from the
    // trace plot below.
    ctx.strokeStyle = 'rgba(148, 163, 184, 0.3)';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(plot.x, plot.y - 0.5);
    ctx.lineTo(plot.x + plot.w, plot.y - 0.5);
    ctx.stroke();

    ctx.restore();
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
    if (!this.axes) return;
    const win = this.renderWindow();
    if (!win) return;
    const { ctx } = this;
    const n = row.length;
    const lut = this.byteToYLut(plot);

    // Map the view window onto a half-open sub-range of FFT bins. At
    // zoom 1 with no pan, this is the whole row; at higher zooms we
    // slice the row down to just the bins inside the visible window
    // and paint *those* across the full plot width. Bin → freq comes
    // from the server's full-span axes; freq → x comes from the view.
    const fullMin = this.axes.centerHz - this.axes.rateHz / 2;
    const fullSpan = this.axes.rateHz;
    const fMin = win.centerHz - win.rateHz / 2;
    const fMax = win.centerHz + win.rateHz / 2;
    const denom = n > 1 ? n - 1 : 1;
    const i0 = Math.max(0, Math.floor(((fMin - fullMin) / fullSpan) * denom));
    const i1 = Math.min(n - 1, Math.ceil(((fMax - fullMin) / fullSpan) * denom));
    if (i1 <= i0) return;
    const sub = row.subarray(i0, i1 + 1);
    const visN = sub.length;

    ctx.beginPath();
    // When more visible bins than pixels (e.g. 16k bins zoomed out to
    // ~1200 px), per-bin lineTo paints multiple bins into the same
    // column and the noise jumps render as a vertical comb. Collapse
    // to per-column max in wasm so the trace is a clean envelope. The
    // reduction runs against `sub` so zooming amplifies bin density
    // smoothly instead of dropping bins.
    const px = Math.max(1, Math.floor(plot.w));
    if (visN > px) {
      if (!this.collapseBuf || this.collapseBuf.length !== px) {
        this.collapseBuf = new Uint8Array(px);
      }
      wasmCollapseRow(sub, this.collapseBuf);
      const cols = this.collapseBuf;
      for (let i = 0; i < px; i++) {
        const x = plot.x + i;
        const y = lut[cols[i]!]!;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
    } else {
      // Direct draw: map each visible bin's centre frequency through
      // the view window so a bin lands exactly under the grid tick at
      // its own frequency, even when the view is panned off-centre.
      const span = fMax - fMin;
      for (let bi = 0; bi < visN; bi++) {
        const fbin = fullMin + ((i0 + bi) / denom) * fullSpan;
        const x = plot.x + ((fbin - fMin) / span) * plot.w;
        const y = lut[sub[bi]!]!;
        if (bi === 0) ctx.moveTo(x, y);
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

/** Trim `text` from the right and append a single ellipsis until it
 *  fits within `maxWidth`. Returns the empty string if even an
 *  ellipsis alone won't fit, so callers can skip the draw entirely. */
function ellipsize(ctx: CanvasRenderingContext2D, text: string, maxWidth: number): string {
  if (maxWidth <= 0) return '';
  if (ctx.measureText(text).width <= maxWidth) return text;
  const ell = '…';
  if (ctx.measureText(ell).width > maxWidth) return '';
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (ctx.measureText(text.slice(0, mid) + ell).width <= maxWidth) lo = mid;
    else hi = mid - 1;
  }
  return lo === 0 ? ell : text.slice(0, lo) + ell;
}

function fmtMHz(hz: number): string {
  const mhz = hz / 1e6;
  const abs = Math.abs(mhz);
  if (abs >= 100) return `${mhz.toFixed(2)}`;
  if (abs >= 1) return `${mhz.toFixed(3)}`;
  return `${mhz.toFixed(4)}`;
}
