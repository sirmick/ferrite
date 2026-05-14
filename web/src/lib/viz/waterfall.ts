// WebGL2 scrolling waterfall.
//
// The history lives in a `cols × rows` R8 texture used as a ring buffer.
// Each incoming FFT row is uploaded with `texSubImage2D` at the current
// write index; no row-shifting, no re-upload of the history. The fragment
// shader unwraps the ring with a single `fract()` using the normalised
// head position, and indexes a 256×1 sigidwiki-style LUT to get the
// final colour.

import { makeSigidwikiLut } from './colormap';

export interface WaterfallOptions {
  /** Number of history rows to keep. Default 512. */
  rows?: number;
  /** Smooth between texels (blurs bins/rows). Default true. */
  linearFilter?: boolean;
}

const VERT_SRC = `#version 300 es
in vec2 a_pos;
out vec2 v_uv;
void main() {
  v_uv = a_pos * 0.5 + 0.5;
  gl_Position = vec4(a_pos, 0.0, 1.0);
}
`;

const FRAG_SRC = `#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 outColor;
uniform sampler2D u_data;
uniform sampler2D u_palette;
uniform float u_head;
// Display-only horizontal zoom/pan: the X texcoord is remapped from
// [0, 1] (screen edges) to [u_xOffset, u_xOffset + u_xScale] inside the
// data texture. At 1× zoom the defaults are 0 and 1, which is a no-op.
uniform float u_xOffset;
uniform float u_xScale;
// Auto-contrast remap: stretch raw byte luminance from
// [u_displayFloor, u_displayCeil] (both in [0, 1] texel space) to
// [0, 1] palette space. Values outside the window clamp to the
// floor/ceil ends so noise-side outliers don't drag the colour ramp.
// Defaults to (0, 1) — a no-op — so the renderer is identical to the
// previous version when auto-contrast is disabled.
uniform float u_displayFloor;
uniform float u_displayCeil;
void main() {
  float offset = 1.0 - v_uv.y;
  float v = fract(u_head - offset);
  float u = u_xOffset + v_uv.x * u_xScale;
  float lum = texture(u_data, vec2(u, v)).r;
  float range = max(u_displayCeil - u_displayFloor, 1.0 / 256.0);
  float stretched = clamp((lum - u_displayFloor) / range, 0.0, 1.0);
  outColor = texture(u_palette, vec2(stretched, 0.5));
}
`;

function compile(gl: WebGL2RenderingContext, type: number, src: string): WebGLShader {
  const sh = gl.createShader(type);
  if (!sh) throw new Error('createShader failed');
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    const log = gl.getShaderInfoLog(sh) ?? '<no log>';
    gl.deleteShader(sh);
    throw new Error(`shader compile failed: ${log}`);
  }
  return sh;
}

function link(gl: WebGL2RenderingContext, vs: WebGLShader, fs: WebGLShader): WebGLProgram {
  const prog = gl.createProgram();
  if (!prog) throw new Error('createProgram failed');
  gl.attachShader(prog, vs);
  gl.attachShader(prog, fs);
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
    const log = gl.getProgramInfoLog(prog) ?? '<no log>';
    gl.deleteProgram(prog);
    throw new Error(`program link failed: ${log}`);
  }
  return prog;
}

/**
 * Map a CSS pixel X-coordinate on the waterfall canvas back to an RF
 * frequency in Hz. The canvas is drawn edge-to-edge (no axis margins),
 * so the conversion is a straight linear blend across the sample-rate
 * span. Pass the view-window centre/rate (when zoomed/panned) so a
 * click lands on the frequency drawn under the cursor — not the full
 * span's frequency. Returns `null` if `cssX` lies outside `[0, widthCss]`.
 */
export function pixelToFreqLinear(
  cssX: number,
  widthCss: number,
  centerHz: number,
  rateHz: number,
): number | null {
  if (!(widthCss > 0)) return null;
  const frac = cssX / widthCss;
  if (!(frac >= 0 && frac <= 1)) return null;
  return centerHz - rateHz / 2 + frac * rateHz;
}

/** Display-only horizontal view window for the waterfall. Maps to
 *  fragment-shader texcoord remapping; no data is re-uploaded. */
export interface WaterfallView {
  /** Centre frequency of the visible slice in Hz. */
  centerHz: number;
  /** Width of the visible slice in Hz. */
  rateHz: number;
  /** Centre frequency of the full data span in Hz (axes.center_freq_hz). */
  fullCenterHz: number;
  /** Width of the full data span in Hz (axes.sample_rate_hz). */
  fullRateHz: number;
}

export class WaterfallRenderer {
  private readonly gl: WebGL2RenderingContext;
  private readonly program: WebGLProgram;
  private readonly vao: WebGLVertexArrayObject;
  private readonly dataTex: WebGLTexture;
  private readonly paletteTex: WebGLTexture;
  private readonly uHead: WebGLUniformLocation;
  private readonly uXOffset: WebGLUniformLocation;
  private readonly uXScale: WebGLUniformLocation;
  private readonly uDisplayFloor: WebGLUniformLocation;
  private readonly uDisplayCeil: WebGLUniformLocation;
  private readonly rows: number;
  private cols = 0;
  private head = 0;
  private rafPending = false;
  private disposed = false;
  private xOffset = 0;
  private xScale = 1;
  // Smoothed display-range floor + ceil in normalised byte units
  // (0 = byte 0, 1 = byte 255). Recomputed from row percentiles in
  // `pushRow` when `autoContrast` is on; EMA-smoothed so it doesn't
  // flicker on noisy ticks. When `autoContrast` is off, these get
  // overwritten by `setManualContrast`.
  private displayFloor = 0;
  private displayCeil = 1;
  // Histogram scratch (256 buckets, one per byte value). Reused
  // across rows to avoid per-frame allocation.
  private readonly hist = new Uint32Array(256);
  // How often (in rows) to recompute the percentile window.
  // 1 = every row; higher = smoother but lags fast band changes.
  private autoContrastEvery = 4;
  private rowCount = 0;
  // Auto-contrast on/off. When off, `manualFloor`/`manualCeil` drive
  // the uniforms directly and the percentile pass is skipped.
  private autoContrast = true;
  private manualFloor = 0;
  private manualCeil = 1;
  // External auto-contrast window — used by the narrow waterfall to
  // share the wide one's auto-stretched bounds, so both panes paint
  // the same byte value with the same palette colour. `null` = no
  // external publisher; the renderer falls back to its own
  // percentile detector. When non-null, the renderer skips its own
  // detector pass and uses these values directly.
  private externalFloor: number | null = null;
  private externalCeil: number | null = null;
  // Callback fired every time the internal detector updates its
  // window. Lets one renderer publish its auto-bounds for another to
  // mirror. Set via `setOnAutoBoundsChanged()`.
  private onAutoBoundsChanged: ((floor: number, ceil: number) => void) | null = null;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    opts: WaterfallOptions = {},
  ) {
    const gl = canvas.getContext('webgl2', { antialias: false, premultipliedAlpha: false });
    if (!gl) throw new Error('WebGL2 not available');
    this.gl = gl;
    this.rows = Math.max(1, Math.floor(opts.rows ?? 512));

    const vs = compile(gl, gl.VERTEX_SHADER, VERT_SRC);
    const fs = compile(gl, gl.FRAGMENT_SHADER, FRAG_SRC);
    this.program = link(gl, vs, fs);
    gl.deleteShader(vs);
    gl.deleteShader(fs);

    const vao = gl.createVertexArray();
    const quad = gl.createBuffer();
    if (!vao || !quad) throw new Error('gl resource alloc failed');
    this.vao = vao;
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, quad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const aPos = gl.getAttribLocation(this.program, 'a_pos');
    gl.enableVertexAttribArray(aPos);
    gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    const dataTex = gl.createTexture();
    const paletteTex = gl.createTexture();
    if (!dataTex || !paletteTex) throw new Error('gl texture alloc failed');
    this.dataTex = dataTex;
    this.paletteTex = paletteTex;

    const filter = (opts.linearFilter ?? true) ? gl.LINEAR : gl.NEAREST;
    gl.bindTexture(gl.TEXTURE_2D, dataTex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    // Vertical wrap makes the ring buffer shader trivial.
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.REPEAT);

    gl.bindTexture(gl.TEXTURE_2D, paletteTex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    // Single-palette LUT — sigidwiki-flavoured rainbow ramp.
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texImage2D(
      gl.TEXTURE_2D,
      0,
      gl.RGBA,
      256,
      1,
      0,
      gl.RGBA,
      gl.UNSIGNED_BYTE,
      makeSigidwikiLut(256),
    );

    const uData = gl.getUniformLocation(this.program, 'u_data');
    const uPalette = gl.getUniformLocation(this.program, 'u_palette');
    const uHead = gl.getUniformLocation(this.program, 'u_head');
    const uXOffset = gl.getUniformLocation(this.program, 'u_xOffset');
    const uXScale = gl.getUniformLocation(this.program, 'u_xScale');
    const uDisplayFloor = gl.getUniformLocation(this.program, 'u_displayFloor');
    const uDisplayCeil = gl.getUniformLocation(this.program, 'u_displayCeil');
    if (!uData || !uPalette || !uHead || !uXOffset || !uXScale || !uDisplayFloor || !uDisplayCeil)
      throw new Error('uniform lookup failed');
    this.uHead = uHead;
    this.uXOffset = uXOffset;
    this.uXScale = uXScale;
    this.uDisplayFloor = uDisplayFloor;
    this.uDisplayCeil = uDisplayCeil;

    gl.useProgram(this.program);
    gl.uniform1i(uData, 0);
    gl.uniform1i(uPalette, 1);
    gl.uniform1f(uXOffset, 0);
    gl.uniform1f(uXScale, 1);
    gl.uniform1f(uDisplayFloor, 0);
    gl.uniform1f(uDisplayCeil, 1);
    gl.useProgram(null);

    this.resize();
  }

  /** Crop the rendered waterfall to a horizontal sub-range of the full
   *  span. Pass `undefined` to revert to full-span. The view is given
   *  in absolute frequencies (centre + width) along with the full
   *  span's centre + width — the renderer converts to texcoord offset
   *  + scale and clamps both so the lookup never reads outside [0, 1]. */
  setView(view: WaterfallView | undefined): void {
    if (this.disposed) return;
    let off = 0;
    let scl = 1;
    if (view && view.fullRateHz > 0) {
      const fullMin = view.fullCenterHz - view.fullRateHz / 2;
      const viewMin = view.centerHz - view.rateHz / 2;
      scl = Math.max(0.0001, Math.min(1, view.rateHz / view.fullRateHz));
      off = Math.max(0, Math.min(1 - scl, (viewMin - fullMin) / view.fullRateHz));
    }
    if (off === this.xOffset && scl === this.xScale) return;
    this.xOffset = off;
    this.xScale = scl;
    this.scheduleDraw();
  }

  /** Upload one FFT row. `row.length` sets the column count on first call. */
  pushRow(row: Uint8Array): void {
    if (this.disposed) return;
    const gl = this.gl;
    if (row.length !== this.cols) this.reallocData(row.length);
    gl.bindTexture(gl.TEXTURE_2D, this.dataTex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, this.head, this.cols, 1, gl.RED, gl.UNSIGNED_BYTE, row);
    this.head = (this.head + 1) % this.rows;
    this.rowCount += 1;
    if (this.autoContrast) {
      if (this.externalFloor !== null && this.externalCeil !== null) {
        // External publisher (the wide waterfall) drives our window.
        // Smoothing happens in the publisher; we just adopt its
        // values so the two panes stay locked together.
        this.displayFloor = this.externalFloor;
        this.displayCeil = this.externalCeil;
      } else if (this.rowCount % this.autoContrastEvery === 0) {
        this.updateAutoContrast(row);
      }
    } else {
      // Manual mode: uniforms snap to the user-set window every row
      // (cheap; just two float writes on the JS side, the draw
      // already runs on rAF). Keeps the renderer's display state in
      // sync if the user toggled away from auto recently.
      this.displayFloor = this.manualFloor;
      this.displayCeil = this.manualCeil;
    }
    this.scheduleDraw();
  }

  /** Switch between auto-contrast (percentile-stretched) and manual
   *  (fixed floor/ceil). When `auto` is true, the renderer recomputes
   *  the window from each row's percentiles; when false, it uses the
   *  values most recently passed to `setManualContrast`. */
  setAutoContrast(auto: boolean): void {
    this.autoContrast = auto;
    this.scheduleDraw();
  }

  /** Set the manual contrast window in normalised byte units
   *  (0 = byte 0, 1 = byte 255). Only takes effect when auto-contrast
   *  is off. */
  setManualContrast(floor: number, ceil: number): void {
    this.manualFloor = Math.max(0, Math.min(1, floor));
    this.manualCeil = Math.max(this.manualFloor + 1 / 256, Math.min(1, ceil));
    if (!this.autoContrast) this.scheduleDraw();
  }

  /** Drive the auto-contrast window from outside instead of running
   *  the internal percentile detector. Pass `null, null` to release
   *  the override and fall back to internal detection. Used by the
   *  narrow waterfall to mirror the wide waterfall's bounds so the
   *  two panes stretch their palette to the same byte window. */
  setExternalAutoBounds(floor: number | null, ceil: number | null): void {
    if (floor === null || ceil === null) {
      this.externalFloor = null;
      this.externalCeil = null;
    } else {
      this.externalFloor = Math.max(0, Math.min(1, floor));
      this.externalCeil = Math.max(this.externalFloor + 1 / 256, Math.min(1, ceil));
    }
    if (this.autoContrast) this.scheduleDraw();
  }

  /** Fires after each internal `updateAutoContrast` pass with the
   *  fresh (post-EMA) floor/ceil in normalised byte units. The wide
   *  waterfall hooks this and republishes via `waterfallStore` so the
   *  narrow renderer can adopt the same window. */
  setOnAutoBoundsChanged(cb: ((floor: number, ceil: number) => void) | null): void {
    this.onAutoBoundsChanged = cb;
  }

  /** Compute P5 / P98 of the row's byte values via histogram and
   *  EMA-smooth them into the display floor/ceil uniforms. The shader
   *  stretches that window to fill the palette — noise floor walks
   *  with the band (HF picks up more atmospheric hash than VHF), and
   *  strong carriers don't compress the noise band into one indistinct
   *  blue smear. */
  private updateAutoContrast(row: Uint8Array): void {
    // O(N) histogram build (one pass through the row).
    this.hist.fill(0);
    for (let i = 0; i < row.length; i++) {
      this.hist[row[i]] += 1;
    }
    // P5 / P95 via cumulative count. Asymmetric window keeps the noise
    // floor visible (low percentile near noise edge) and saturates only
    // the brightest few percent of bins (strong carrier whites).
    const total = row.length;
    const loCount = Math.floor(total * 0.05);
    const hiCount = Math.floor(total * 0.98);
    let cum = 0;
    let pLo = 0;
    let pHi = 255;
    for (let b = 0; b < 256; b++) {
      cum += this.hist[b];
      if (pLo === 0 && cum >= loCount) pLo = b;
      if (cum >= hiCount) {
        pHi = b;
        break;
      }
    }
    // Floor a tiny bit below P5 so the noise floor sits in the
    // dark-blue band, not pure black. Ceil a hair above P95 so strong
    // carriers actually reach the bright end of the palette.
    let floor = Math.max(0, pLo - 4) / 255;
    let ceil = Math.min(255, pHi + 6) / 255;
    // Guard against a degenerate flat row (all-zero / all-same byte)
    // which would collapse the window to a point — fall back to no
    // stretch in that case.
    if (ceil - floor < 0.05) {
      floor = 0;
      ceil = 1;
    }
    // EMA against the previous window. 0.3 is fast enough to follow a
    // tune across a band edge in ~5 row updates but slow enough that
    // a single bursty signal doesn't shove the floor up.
    const alpha = 0.3;
    this.displayFloor = this.displayFloor * (1 - alpha) + floor * alpha;
    this.displayCeil = this.displayCeil * (1 - alpha) + ceil * alpha;
    // Publish the smoothed window for any subscribers (e.g. the
    // narrow waterfall mirroring the wide one's bounds).
    this.onAutoBoundsChanged?.(this.displayFloor, this.displayCeil);
  }

  /** Resize the drawing buffer to the CSS size. Call on resize. */
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
    this.gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    this.scheduleDraw();
  }

  destroy(): void {
    if (this.disposed) return;
    this.disposed = true;
    const gl = this.gl;
    gl.deleteTexture(this.dataTex);
    gl.deleteTexture(this.paletteTex);
    gl.deleteVertexArray(this.vao);
    gl.deleteProgram(this.program);
  }

  private reallocData(cols: number): void {
    const gl = this.gl;
    this.cols = cols;
    this.head = 0;
    gl.bindTexture(gl.TEXTURE_2D, this.dataTex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    // Allocate with zeros so pre-fill rows draw as the floor colour.
    const zeros = new Uint8Array(cols * this.rows);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.R8, cols, this.rows, 0, gl.RED, gl.UNSIGNED_BYTE, zeros);
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
    if (this.disposed || this.cols === 0) return;
    const gl = this.gl;
    gl.useProgram(this.program);
    gl.bindVertexArray(this.vao);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.dataTex);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.paletteTex);
    gl.uniform1f(this.uHead, this.head / this.rows);
    gl.uniform1f(this.uXOffset, this.xOffset);
    gl.uniform1f(this.uXScale, this.xScale);
    gl.uniform1f(this.uDisplayFloor, this.displayFloor);
    gl.uniform1f(this.uDisplayCeil, this.displayCeil);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    gl.bindVertexArray(null);
    gl.useProgram(null);
  }
}
