// Ambient types for the generated sherpa-onnx Emscripten ASR modules.
//
// `sherpa-onnx-wasm-main-asr.{js,wasm,data}` and the API glue
// `sherpa-onnx-asr.js` are emitted by
// blocks/native/sherpa/emscripten/build.sh (pnpm wasm:build:sherpa) and
// are git-ignored — the real `.js` (minified) is not present at
// type-check time until emsdk + sherpa-onnx sources are vendored. These
// wildcard declarations let svelte-check / tsc resolve the dynamic
// `import('.../sherpa-onnx-*.js')` in sherpaEngine.ts regardless, keeping
// the build graceful when the artifact is absent (mirrors whisper.d.ts /
// fldigi.d.ts). At runtime the import fails and SherpaEngine.load throws
// EngineUnavailableError; audio is unaffected (passthrough).

/** The Emscripten module factory (`-sMODULARIZE=1 -sEXPORT_ES6=1`); the
 *  default export, awaited, yields the instantiated Module. */
declare module '*/sherpa-onnx-wasm-main-asr.js' {
  const createSherpaModule: (
    moduleArg?: Record<string, unknown>,
  ) => Promise<Record<string, unknown>>;
  export default createSherpaModule;
}

/** The hand-shipped JS API glue over the module — builds the streaming
 *  recognizer. Typed loosely; sherpaEngine.ts casts to the shapes it
 *  uses. */
declare module '*/sherpa-onnx-asr.js' {
  export function createOnlineRecognizer(mod: Record<string, unknown>): unknown;
}
