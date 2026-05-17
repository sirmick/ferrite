// Ambient type for the generated Emscripten fldigi module.
//
// `fldigi.{mjs,wasm}` are emitted by
// blocks/native/fldigi/emscripten/build.sh (pnpm wasm:build:fldigi)
// and are git-ignored — so the real .mjs (minified, `// @ts-nocheck`)
// is not always present at type-check time. This wildcard module
// declaration lets svelte-check / tsc resolve `import('./fldigi.mjs')`
// regardless, keeping the build graceful when emsdk hasn't run.
//
// build.sh uses `-sMODULARIZE=1 -sEXPORT_ES6=1
// -sEXPORT_NAME=createFldigiModule`: the default export is the module
// factory; calling it returns a Promise resolving to the instantiated
// Emscripten Module (heap + exported `_fldigi_*` ABI). The bridge
// treats the Module as opaque (`unknown`) and hands it to the
// wasm-bindgen snippet via `globalThis.__FERRITE_FLDIGI__`.
declare module '*/fldigi.mjs' {
  const createFldigiModule: (moduleArg?: Record<string, unknown>) => Promise<unknown>;
  export default createFldigiModule;
}
