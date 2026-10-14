// @ts-nocheck — copied verbatim by wasm-bindgen into `snippets/`.
//
// JS side of `ferrite-mbelib`'s wasm32 backend. Talks only to the
// sibling Emscripten vocoder module published on
// `globalThis.__FERRITE_MBE__` by web/src/lib/wasm/vocoder/mbeBridge.ts,
// which the browser runner loads *lazily* — only for flowgraphs that
// decode voice in the browser. Until then (or if the module isn't
// built) every call is inert: mbe_create() returns 0, the Rust
// `Vocoder::new` yields None, and the decoder emits silence.

function M() {
  return (typeof globalThis !== 'undefined' && globalThis.__FERRITE_MBE__) || null;
}

export function mbe_create() {
  const m = M();
  return m ? m._ferrite_mbe_create() >>> 0 : 0;
}

// Returns 160 samples followed by the error count (161 floats).
export function mbe_decode(h, fr96) {
  const m = M();
  if (!m || !h) return new Float32Array(0);
  const pf = m._malloc(96);
  const po = m._malloc(160 * 4);
  m.HEAPU8.set(fr96, pf);
  const errs = m._ferrite_mbe_decode_ambe2450(h, pf, po);
  const out = new Float32Array(161);
  out.set(m.HEAPF32.subarray(po >> 2, (po >> 2) + 160));
  out[160] = errs;
  m._free(pf);
  m._free(po);
  return out;
}

export function mbe_destroy(h) {
  const m = M();
  if (m && h) m._ferrite_mbe_destroy(h);
}
