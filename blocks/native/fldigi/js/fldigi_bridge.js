// @ts-nocheck — wasm-bindgen copies this verbatim into the generated
// `snippets/` dir; it is plain bridge JS (numbers + typed arrays into
// the Emscripten heap), not part of the typed TS surface.
//
// wasm-bindgen module imported by the wasm32 backend of
// `ferrite-fldigi` (see src/lib.rs `#[wasm_bindgen(module = ...)]`).
//
// Each function below is the JS side of one ABI call. wasm-bindgen
// marshals Rust↔JS (string/&[f32]/String/Vec<f32>) so there is NO
// raw-pointer or wasm-memory juggling here — we only talk to the
// sibling Emscripten fldigi module.
//
// The Emscripten module is provided out-of-band via a global the
// worker sets (`globalThis.__FERRITE_FLDIGI__`) — see
// web/src/lib/wasm/fldigi/fldigiBridge.ts. This avoids depending on
// the hashed wasm-bindgen `snippets/` path: this file has zero
// imports. If the global is absent (Emscripten not built / not
// initialised) every call is inert: `fldigi_create` returns 0, the
// Rust `FldigiModem::new` yields `None`, and the preset is simply
// unavailable in-browser (node-side decode unaffected). Nothing throws.

function M() {
  // The Emscripten Module instance, or null when unavailable.
  return (typeof globalThis !== 'undefined' && globalThis.__FERRITE_FLDIGI__) || null;
}

export function fldigi_create(mode, sampleRate) {
  const m = M();
  if (!m) return 0;
  const len = m.lengthBytesUTF8(mode) + 1;
  const p = m._malloc(len);
  m.stringToUTF8(mode, p, len);
  const h = m._fldigi_modem_create(p, sampleRate);
  m._free(p);
  return h >>> 0;
}

export function fldigi_rx(h, audio) {
  const m = M();
  if (!m || !h || audio.length === 0) return;
  const bytes = audio.length * 4;
  const p = m._malloc(bytes);
  m.HEAPF32.set(audio, p >> 2);
  m._fldigi_modem_rx(h, p, audio.length);
  m._free(p);
}

export function fldigi_set_param(h, key, value) {
  const m = M();
  if (!m || !h) return;
  const len = m.lengthBytesUTF8(key) + 1;
  const p = m._malloc(len);
  m.stringToUTF8(key, p, len);
  m._fldigi_modem_set_param(h, p, value);
  m._free(p);
}

// Drain a byte buffer (text/status) fully into a JS string.
function drainStr(emFn, h) {
  const m = M();
  if (!m || !h) return '';
  const cap = 4096;
  const p = m._malloc(cap);
  let out = '';
  for (;;) {
    const n = m[emFn](h, p, cap);
    if (n <= 0) break;
    out += new TextDecoder().decode(m.HEAPU8.subarray(p, p + Math.min(n, cap)));
    if (n < cap) break;
  }
  m._free(p);
  return out;
}

export function fldigi_drain_text(h) {
  return drainStr('_fldigi_modem_drain_text', h);
}
export function fldigi_drain_status(h) {
  return drainStr('_fldigi_modem_drain_status', h);
}

// Scope: returns the latest frame's samples; mode fetched separately
// (kept after drain so fldigi_scope_mode(h) reflects it).
let lastScopeMode = 0;
export function fldigi_drain_scope(h) {
  const m = M();
  if (!m || !h) return new Float32Array(0);
  const cap = 8192;
  const ob = m._malloc(cap * 4);
  const mb = m._malloc(4);
  const n = m._fldigi_modem_drain_scope(h, ob, cap, mb);
  let out = new Float32Array(0);
  if (n > 0) {
    out = m.HEAPF32.slice(ob >> 2, (ob >> 2) + Math.min(n, cap));
    lastScopeMode = m.HEAP32[mb >> 2] | 0;
  }
  m._free(ob);
  m._free(mb);
  return out;
}
export function fldigi_scope_mode(_h) {
  return lastScopeMode | 0;
}

export function fldigi_destroy(h) {
  const m = M();
  if (m && h) m._fldigi_modem_destroy(h);
}
