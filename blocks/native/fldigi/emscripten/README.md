# fldigi wasm bridge (link-vs-bridge)

fldigi is C++/STL. It is **not linked** into the Rust/wasm-bindgen
module. Instead the curated cores are built once by Emscripten into a
sibling `fldigi.wasm` (full libc++ + exceptions, for free), and the
two modules are joined at runtime by a narrow **pull/drain C ABI** +
buffer copies. Two linear memories, never linked.

## Pieces (status)

| Piece | Where | State |
|---|---|---|
| Pull/drain C ABI | `src/lib.rs`, `shim/fldigi_shim.cxx` | ✅ done, native-verified |
| Native backend (static link) | `build.rs` (non-wasm) | ✅ done, tests green |
| wasm = ABI as imports (no link) | `build.rs` (wasm32 → compile nothing) | ✅ done |
| Emscripten build recipe | `emscripten/build.sh` | ✅ written; inert until `emcc` |
| JS marshalling thunks | `web/src/lib/wasm/fldigi/fldigiBridge.ts` | ✅ complete + correct |
| **Import injection seam** | wasm-bindgen instantiation | ⏳ closes with the emsdk spike |

## Why pull/drain, not callbacks

A function pointer in the Rust module is meaningless to the Emscripten
module — distinct function tables and linear memories. The C side
accumulates decoded text/scope/image internally; the host drains via
`fldigi_modem_drain_*`. This is the *one* ABI shape identical native
and bridged (and it's what the Rust wrapper already did internally).

## The one open seam

`fldigiBridge.ts::fldigiEnvImports()` returns the `env` functions the
Rust module imports for the fldigi ABI; `bindRustMemory()` gives the
thunks the Rust instance's memory. The marshalling is complete and
correct. The only unresolved bit — because it cannot be validated
without `emcc` — is **merging those imports into the wasm-bindgen
`--target web` import object**, which `__wbg_init` builds internally
(`web/src/lib/wasm/runtime/runtime.js:__wbg_get_imports`). Options the
spike will pick between (with the artifact in hand):

1. instantiate via the lower-level wasm-bindgen path that accepts a
   caller-supplied `imports` object, merging `fldigiEnvImports()`;
2. post-process the generated glue to splice the `env` namespace;
3. a `build.rs` wasm shim that re-exports the ABI through `#[wasm_bindgen]`
   so the symbols land in the `wbg` namespace wasm-bindgen already owns.

(3) is likely cleanest. None can be verified until emsdk is installed
(CI image / dev box — deliberately, not silently in a sandbox).

## Running it once emsdk exists

```
pnpm wasm:build        # wasm:build:fldigi runs the script below;
                       # skips with a notice if emcc is absent
blocks/native/fldigi/emscripten/build.sh   # or directly
```

Native fldigi decode is fully working today and unaffected by any of
this — the bridge only gates *in-browser* decode; shipped presets run
the decoder `node`-side regardless.
