// Viridis colormap — a perceptually-uniform palette. We use Matt Zucker's
// published 6th-order polynomial fit of the reference table; the error vs.
// the matplotlib LUT is a couple of LSBs and the coefficients weigh 160
// bytes instead of 1 KB for a 256-entry table.

const VIRIDIS_COEF: ReadonlyArray<readonly [number, number, number]> = [
  [0.277727327223418, 0.005407344544967, 0.334099805335306],
  [0.105093043108577, 1.404613529898567, 1.384590162594685],
  [-0.330861828725556, 0.214847559468213, 0.095095163028237],
  [-4.634230498983486, -5.799100973351585, -19.33244095627987],
  [6.228269936347081, 14.17993336680509, 56.69055260068105],
  [4.776384997670288, -13.74514537774601, -65.35303263337234],
  [-5.435455855934631, 4.645852612178535, 26.3124352495832],
];

function clamp255(x: number): number {
  if (x <= 0) return 0;
  if (x >= 255) return 255;
  return Math.round(x);
}

/** 256 × RGBA8, suitable for a WebGL2 `LUMINANCE`-indexed palette texture. */
export function makeViridisLut(n = 256): Uint8Array {
  const out = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    const t = n === 1 ? 0 : i / (n - 1);
    let r = 0;
    let g = 0;
    let b = 0;
    for (let k = VIRIDIS_COEF.length - 1; k >= 0; k--) {
      const c = VIRIDIS_COEF[k];
      r = r * t + c[0];
      g = g * t + c[1];
      b = b * t + c[2];
    }
    out[i * 4 + 0] = clamp255(r * 255);
    out[i * 4 + 1] = clamp255(g * 255);
    out[i * 4 + 2] = clamp255(b * 255);
    out[i * 4 + 3] = 255;
  }
  return out;
}
