// Worker-side initialiser for the fldigi Emscripten bridge.
//
// The wasm32 backend of `ferrite-fldigi` imports its ABI from the
// wasm-bindgen snippet `js/fldigi_bridge.js`, which reads the
// Emscripten module off `globalThis.__FERRITE_FLDIGI__` (snippet has
// zero imports → no hashed-path coupling). This module owns that
// instantiation: the worker calls `initFldigiBridge()` once, before
// running any flowgraph, so the snippet's synchronous calls find a
// ready module.
//
// `fldigi.{mjs,wasm}` is emitted by
// blocks/native/fldigi/emscripten/build.sh (pnpm wasm:build:fldigi).
// If it's absent (no emsdk) the dynamic import fails, the global stays
// unset, and every bridge call is inert: in-browser fldigi presets are
// unavailable but nothing throws and node-side decode is unaffected.

let initPromise: Promise<boolean> | undefined;

/** Instantiate the Emscripten fldigi module and publish it for the
 *  wasm-bindgen snippet. Idempotent; returns whether the bridge is
 *  live. Safe to await unconditionally. */
export function initFldigiBridge(): Promise<boolean> {
  if (!initPromise) {
    initPromise = (async () => {
      try {
        // Built artifact sits next to this file. @vite-ignore: the
        // file may not exist until `pnpm wasm:build:fldigi` runs.
        const mod: { default: () => Promise<unknown> } = await import(
          /* @vite-ignore */ './fldigi.mjs'
        );
        const M = await mod.default();
        (globalThis as Record<string, unknown>).__FERRITE_FLDIGI__ = M;
        return true;
      } catch {
        return false; // inert: preset unavailable in-browser, no throw
      }
    })();
  }
  return initPromise;
}

/** True once the Emscripten module is live (after `initFldigiBridge`). */
export function fldigiBridgeReady(): boolean {
  return Boolean((globalThis as Record<string, unknown>).__FERRITE_FLDIGI__);
}
