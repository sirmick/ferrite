// Waterfall palette.
//
// Single ramp matched to the spectrograms on sigidwiki / Artemis — the
// classic SDR# / GQRX "jet"-flavoured rainbow. It's not perceptually
// uniform like viridis, but it's what every signal sample on the wiki
// looks like, which makes A/B-ing a captured signal against the wiki
// reference visually trivial.
//
// Stops were eyeballed from a sigidwiki POCSAG waterfall thumbnail
// (sampled along its brightest row): black → deep navy at the noise
// floor, walking through royal blue / cyan / green / yellow / orange
// / red, ending at white at the strongest peaks. Linear interpolation
// between anchors.

const SIGIDWIKI_STOPS: ReadonlyArray<readonly [number, number, number, number]> = [
  // pos     R     G     B   (RGB in 0–1)
  [0.0, 0.0, 0.0, 0.0], // black — below the floor
  [0.06, 0.0, 0.0, 0.25], // very dark navy
  [0.12, 0.0, 0.0, 0.5], // deep navy
  [0.22, 0.05, 0.24, 0.74], // royal blue
  [0.36, 0.31, 0.65, 0.78], // cyan-blue
  [0.5, 0.5, 0.75, 0.57], // teal-green
  [0.62, 0.86, 0.87, 0.17], // yellow-green
  [0.74, 0.99, 0.81, 0.06], // gold
  [0.84, 0.99, 0.49, 0.07], // orange
  [0.94, 0.9, 0.04, 0.01], // red
  [1.0, 1.0, 1.0, 1.0], // white peak
];

function clamp255(x: number): number {
  if (x <= 0) return 0;
  if (x >= 255) return 255;
  return Math.round(x);
}

/** 256 × RGBA8 LUT for the sigidwiki-flavoured waterfall ramp.
 *  Suitable for a WebGL2 `LUMINANCE`-indexed palette texture. Linear
 *  interpolation between [`SIGIDWIKI_STOPS`] anchors. */
export function makeSigidwikiLut(n = 256): Uint8Array {
  const out = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    const t = n === 1 ? 0 : i / (n - 1);
    let lo = SIGIDWIKI_STOPS[0]!;
    let hi = SIGIDWIKI_STOPS[SIGIDWIKI_STOPS.length - 1]!;
    for (let k = 0; k < SIGIDWIKI_STOPS.length - 1; k++) {
      const a = SIGIDWIKI_STOPS[k]!;
      const b = SIGIDWIKI_STOPS[k + 1]!;
      if (t >= a[0] && t <= b[0]) {
        lo = a;
        hi = b;
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
