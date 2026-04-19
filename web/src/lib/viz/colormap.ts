// Waterfall palettes.
//
// Two palettes live here:
//   • `makeDigiLut()` — high-contrast classic DigiPan / WebSDR-style ramp,
//     black → navy → blue → cyan → green → yellow → red → white. Makes
//     signals pop against noise; default for the waterfall.
//   • `makeViridisLut()` — matplotlib's perceptually-uniform palette (Matt
//     Zucker's polynomial fit). Kept for callers that want it.

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

// DigiPan / "classic SDR" colour stops. Position is in [0,1]; RGB is 0–1.
// Tuned so the low end stays dark-dark (noise floor disappears) and signal
// energy walks through a bright rainbow.
const DIGI_STOPS: ReadonlyArray<readonly [number, number, number, number]> = [
  [0.0, 0.0, 0.0, 0.0], // black
  [0.1, 0.0, 0.0, 0.12], // near-black navy
  [0.25, 0.0, 0.0, 0.75], // blue
  [0.4, 0.0, 0.7, 1.0], // cyan-blue
  [0.55, 0.0, 1.0, 0.35], // green
  [0.7, 1.0, 1.0, 0.0], // yellow
  [0.85, 1.0, 0.2, 0.0], // red-orange
  [1.0, 1.0, 1.0, 1.0], // white
];

/**
 * 256 × RGBA8 LUT for the DigiPan-style palette. Linear interpolation
 * between the [`DIGI_STOPS`] anchors.
 */
export function makeDigiLut(n = 256): Uint8Array {
  const out = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    const t = n === 1 ? 0 : i / (n - 1);
    let lo = DIGI_STOPS[0];
    let hi = DIGI_STOPS[DIGI_STOPS.length - 1];
    for (let k = 0; k < DIGI_STOPS.length - 1; k++) {
      if (t >= DIGI_STOPS[k][0] && t <= DIGI_STOPS[k + 1][0]) {
        lo = DIGI_STOPS[k];
        hi = DIGI_STOPS[k + 1];
        break;
      }
    }
    const span = hi[0] - lo[0];
    const u = span > 0 ? (t - lo[0]) / span : 0;
    out[i * 4 + 0] = clamp255((lo[1] + (hi[1] - lo[1]) * u) * 255);
    out[i * 4 + 1] = clamp255((lo[2] + (hi[2] - lo[2]) * u) * 255);
    out[i * 4 + 2] = clamp255((lo[3] + (hi[3] - lo[3]) * u) * 255);
    out[i * 4 + 3] = 255;
  }
  return out;
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
