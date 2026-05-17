// Ambient type for the generated Emscripten whisper module.
//
// `whisper.{mjs,wasm}` are emitted by
// blocks/native/whisper/emscripten/build.sh (pnpm wasm:build:whisper)
// and are git-ignored — the real .mjs (minified, `// @ts-nocheck`) is
// not present at type-check time until emsdk + whisper.cpp sources are
// vendored. This wildcard declaration lets svelte-check / tsc resolve
// `import('./whisper.mjs')` regardless, keeping the build graceful when
// the artifact is absent (mirrors fldigi.d.ts).
//
// build.sh uses `-sMODULARIZE=1 -sEXPORT_ES6=1
// -sEXPORT_NAME=createWhisperModule`. The default export is the module
// factory; awaiting it yields the instantiated Emscripten Module with
// the C glue ABI declared in
// blocks/native/whisper/shim/whisper_glue.c:
//
//   _wsp_init(modelPtr,modelLen, vadPtr,vadLen) -> int   (1 = ok)
//   _wsp_transcribe(pcmPtr, nSamples, promptPtr) -> char* (malloc'd
//        JSON; caller frees) — see WhisperResult for the shape
//   _wsp_vad_available() -> int
//   _malloc / _free
declare module '*/whisper.mjs' {
  interface WhisperModule {
    _wsp_init(modelPtr: number, modelLen: number, vadPtr: number, vadLen: number): number;
    _wsp_transcribe(pcmPtr: number, nSamples: number, promptPtr: number): number;
    _wsp_vad_available(): number;
    _malloc(bytes: number): number;
    _free(ptr: number): void;
    HEAPU8: Uint8Array;
    HEAPF32: Float32Array;
    stringToUTF8(s: string, ptr: number, max: number): void;
    lengthBytesUTF8(s: string): number;
    UTF8ToString(ptr: number): string;
  }
  const createWhisperModule: (moduleArg?: Record<string, unknown>) => Promise<WhisperModule>;
  export default createWhisperModule;
}
