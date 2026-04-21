// WebGL2 scrolling waterfall.
//
// The history lives in a `cols × rows` R8 texture used as a ring buffer.
// Each incoming FFT row is uploaded with `texSubImage2D` at the current
// write index; no row-shifting, no re-upload of the history. The fragment
// shader unwraps the ring with a single `fract()` using the normalised
// head position, and indexes a 256×1 viridis LUT to get the final colour.

import { makeDigiLut, makeViridisLut } from './colormap';

export type WaterfallPalette = 'digi' | 'viridis';

export interface WaterfallOptions {
  /** Number of history rows to keep. Default 512. */
  rows?: number;
  /** Smooth between texels (blurs bins/rows). Default true. */
  linearFilter?: boolean;
  /** Colour palette. Default `'digi'`. */
  palette?: WaterfallPalette;
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
void main() {
  // Screen y=1 is the top of the panel; the newest row should land there.
  // Ring head is the NEXT write slot, so newest sample sits just below it.
  float offset = 1.0 - v_uv.y;
  float v = fract(u_head - offset);
  float lum = texture(u_data, vec2(v_uv.x, v)).r;
  outColor = texture(u_palette, vec2(lum, 0.5));
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
 * span. Returns `null` if `cssX` lies outside `[0, widthCss]`.
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

export class WaterfallRenderer {
  private readonly gl: WebGL2RenderingContext;
  private readonly program: WebGLProgram;
  private readonly vao: WebGLVertexArrayObject;
  private readonly dataTex: WebGLTexture;
  private readonly paletteTex: WebGLTexture;
  private readonly uHead: WebGLUniformLocation;
  private readonly rows: number;
  private cols = 0;
  private head = 0;
  private rafPending = false;
  private disposed = false;
  private palette: WaterfallPalette = 'digi';

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
    this.palette = opts.palette ?? 'digi';
    this.uploadPalette();

    const uData = gl.getUniformLocation(this.program, 'u_data');
    const uPalette = gl.getUniformLocation(this.program, 'u_palette');
    const uHead = gl.getUniformLocation(this.program, 'u_head');
    if (!uData || !uPalette || !uHead) throw new Error('uniform lookup failed');
    this.uHead = uHead;

    gl.useProgram(this.program);
    gl.uniform1i(uData, 0);
    gl.uniform1i(uPalette, 1);
    gl.useProgram(null);

    this.resize();
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
    this.scheduleDraw();
  }

  /** Swap the colour palette. Re-uploads the 256×1 LUT texture. */
  setPalette(palette: WaterfallPalette): void {
    if (this.disposed || this.palette === palette) return;
    this.palette = palette;
    this.uploadPalette();
    this.scheduleDraw();
  }

  private uploadPalette(): void {
    const gl = this.gl;
    const lut = this.palette === 'viridis' ? makeViridisLut(256) : makeDigiLut(256);
    gl.bindTexture(gl.TEXTURE_2D, this.paletteTex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 256, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, lut);
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
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    gl.bindVertexArray(null);
    gl.useProgram(null);
  }
}
