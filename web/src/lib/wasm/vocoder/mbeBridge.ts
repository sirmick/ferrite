// Lazy loader for the browser vocoder (mbelib AMBE+2 / IMBE synthesis).
//
// `ferrite-mbelib`'s wasm32 backend imports its ABI from the
// wasm-bindgen snippet `js/mbe_bridge.js`, which reads the Emscripten
// module off `globalThis.__FERRITE_MBE__`. This module owns that
// instantiation, and the runner calls it *only* when a flowgraph puts a
// voice decoder (DmrDecode) on the browser side — so pages that never
// decode digital voice never fetch `mbe.wasm`. That is the lazy-load
// seam for this and any future proprietary codec module.
//
// `mbe.{mjs,wasm}` come from blocks/native/mbelib/emscripten/build.sh
// (`pnpm wasm:build:vocoder`; git-ignored). If absent, the glob is empty,
// the global stays unset, and the decoder emits silence — call metadata
// is unaffected.

let initPromise: Promise<boolean> | undefined;

const mbeModules = import.meta.glob<{
  default: (moduleArg?: Record<string, unknown>) => Promise<Record<string, unknown>>;
}>('./mbe.mjs');

/** Instantiate the vocoder module and publish it for the snippet.
 *  Idempotent; resolves to whether voice synthesis is available.
 *  Node/vitest callers pass `{ wasmBinary }` (the default `fetch` of the
 *  sibling `.wasm` isn't available there). */
export function initMbeBridge(moduleArg?: Record<string, unknown>): Promise<boolean> {
  if (!initPromise) {
    initPromise = (async () => {
      try {
        const load = mbeModules['./mbe.mjs'];
        if (!load) return false;
        const mod = await load();
        const M = await mod.default(moduleArg);
        (globalThis as Record<string, unknown>).__FERRITE_MBE__ = M;
        return true;
      } catch {
        return false;
      }
    })();
  }
  return initPromise;
}

/** True once the vocoder module is live. */
export function mbeBridgeReady(): boolean {
  return Boolean((globalThis as Record<string, unknown>).__FERRITE_MBE__);
}
