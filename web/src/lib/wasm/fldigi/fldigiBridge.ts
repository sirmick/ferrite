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
 *  live. Safe to await unconditionally.
 *
 *  `moduleArg` is merged into the Emscripten `Module` (e.g. pass
 *  `{ wasmBinary }` to instantiate from in-memory bytes instead of the
 *  default `fetch(new URL('fldigi.wasm', import.meta.url))`). Browser
 *  callers pass nothing — the fetch path resolves the vite-emitted
 *  asset; only the node/vitest e2e injects bytes (its module-URL
 *  scheme isn't a real fetch/file path). */
export function initFldigiBridge(moduleArg?: Record<string, unknown>): Promise<boolean> {
  if (!initPromise) {
    initPromise = (async () => {
      try {
        // fldigi.{mjs,wasm} are emitted by `pnpm wasm:build:fldigi`
        // (emsdk; git-ignored). Literal specifier so vite bundles the
        // Emscripten ESM module and emits its
        // `new URL('fldigi.wasm', import.meta.url)` sidecar as a
        // hashed asset. Types resolve via the committed sibling
        // `fldigi.d.ts` ambient declaration, so svelte-check passes
        // whether or not the generated file is present.
        const mod = await import('./fldigi.mjs');
        const M = await mod.default(moduleArg);
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
