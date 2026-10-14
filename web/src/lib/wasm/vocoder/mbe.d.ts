// Type shim for the generated (git-ignored) Emscripten vocoder module —
// see mbeBridge.ts. `-sMODULARIZE -sEXPORT_ES6 -sEXPORT_NAME=createMbeModule`:
// the default export is the module factory.
declare module '*/mbe.mjs' {
  const createMbeModule: (moduleArg?: Record<string, unknown>) => Promise<unknown>;
  export default createMbeModule;
}
