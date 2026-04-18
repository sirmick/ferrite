// Canvas2D line plot of the most-recent FFT row.
//
// Deliberately simple — one canvas, one path per frame. A 4096-bin row is
// ~4K line segments, which canvas2d chews through in <1 ms on a modern
// laptop GPU. If this ever becomes a bottleneck we can port it to WebGL
// without changing the Svelte wrapper.

export interface SpectrumOptions {
  /** CSS colour of the trace. Defaults to the app accent. */
  color?: string;
  /** CSS colour of the subtle zero-line. Defaults to transparent. */
  baselineColor?: string;
}

export class SpectrumRenderer {
  private readonly ctx: CanvasRenderingContext2D;
  private row: Uint8Array | undefined;
  private rafPending = false;
  private disposed = false;
  private color: string;
  private baselineColor: string;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    opts: SpectrumOptions = {},
  ) {
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas2d not available');
    this.ctx = ctx;
    this.color = opts.color ?? '#7dd3fc';
    this.baselineColor = opts.baselineColor ?? 'rgba(125, 211, 252, 0.12)';
    this.resize();
  }

  setRow(row: Uint8Array): void {
    if (this.disposed) return;
    this.row = row;
    this.scheduleDraw();
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
    const w = this.canvas.width;
    const h = this.canvas.height;
    ctx.clearRect(0, 0, w, h);
    const row = this.row;
    if (!row || row.length === 0) return;

    // Baseline at the bottom (value 0).
    ctx.strokeStyle = this.baselineColor;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, h - 0.5);
    ctx.lineTo(w, h - 0.5);
    ctx.stroke();

    const n = row.length;
    ctx.strokeStyle = this.color;
    ctx.lineWidth = Math.max(1, Math.floor(Math.min(window.devicePixelRatio || 1, 2)));
    ctx.beginPath();
    const xScale = w / (n - 1);
    // Row value 0 → bottom (h), value 255 → top (0).
    const yScale = h / 255;
    for (let i = 0; i < n; i++) {
      const x = i * xScale;
      const y = h - row[i] * yScale;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
}
