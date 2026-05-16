//! Build script for the curated fldigi modem cores.
//!
//! ## The link-vs-bridge design (see the approved plan)
//!
//! fldigi is C++ with heavy STL. There is no C++/STL →
//! `wasm32-unknown-unknown` precedent in this repo (every other
//! vendored block is C; `libc-stubs/` is a *C* libc only — no
//! libc++/libc++abi). Trying to *link* this into the Rust/wasm-bindgen
//! module would force a freestanding-libc++ port. We don't link. We
//! bridge:
//!
//! * **Native** (`cargo build`): compile the curated C++ subset with
//!   `cc::Build` and statically link it — exactly the wsprd pattern.
//!   The `extern "C"` ABI in `src/lib.rs` is resolved by `ld`.
//!
//! * **wasm32** (`wasm-pack`): compile *nothing* here. The same
//!   `extern "C"` symbols become undefined → LLVM emits them as wasm
//!   **imports**. They are satisfied at instantiation by JS thunks in
//!   `web/src/lib/runner/runnerWorker.ts` that marshal into a sibling
//!   **Emscripten** `fldigi.wasm` (its own linear memory + libc++ +
//!   exceptions, all free). Two modules, never linked, joined by a
//!   narrow C ABI + buffer copy.
//!
//! So the "linker" is `ld` (native) or the JS instantiation glue
//! (wasm). One Rust block, `Placement::Either`, two backends.
//!
//! Curation is iterative: drop a `.cxx` into `vendor/` (or extend a
//! shim header) until the native link is clean. Every `.cxx`/`.cpp`
//! under `vendor/` and `shim/` is compiled; includes must resolve
//! through `shim/` (first — replacement headers win) then `vendor/`
//! and its subdirs.

use std::path::{Path, PathBuf};

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");

    println!("cargo:rerun-if-changed=build.rs");
    for dir in [&shim, &vendor] {
        for f in walk(dir) {
            println!("cargo:rerun-if-changed={}", f.display());
        }
    }

    if target.starts_with("wasm32") {
        // Bridge path: compile nothing. The `extern "C"` fldigi ABI
        // in src/lib.rs stays undefined and LLVM emits it as wasm
        // imports, satisfied by the sibling Emscripten module via JS
        // thunks. The Emscripten artifact is built by a separate step
        // (blocks/native/fldigi/emscripten/, wired into pnpm
        // wasm:build) — intentionally NOT this build script.
        return;
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        // shim/ FIRST: our replacement fl_digi.h / configuration.h /
        // status.h / qrunner.h / config.h must win over fldigi's.
        .include(&shim)
        .include(&vendor)
        .warnings(false)
        // fldigi genuinely uses C++ exceptions (SndException) and RTTI.
        // The earlier -fno-exceptions/-fno-rtti were artifacts of the
        // abandoned freestanding-libc++ wasm plan; the chosen Emscripten
        // bridge supports full EH, and the native link has no reason to
        // forbid it. Keep them enabled.
        .flag_if_supported("-Wno-deprecated");
    for inc in vendor_include_dirs(&vendor) {
        build.include(inc);
    }

    // A few vendored fldigi `.cxx` are *include fragments*, not
    // translation units — they're `#include`d into a sibling and must
    // not be compiled standalone (`rsid_defs.cxx` holds the RSID
    // tables and is pulled into `rsid.cxx`). Skip by name.
    const INCLUDE_FRAGMENTS: &[&str] = &["rsid_defs.cxx"];

    let mut n = 0;
    for src in walk(&shim).into_iter().chain(walk(&vendor)) {
        if src
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| INCLUDE_FRAGMENTS.contains(&f))
        {
            continue;
        }
        if matches!(
            src.extension().and_then(|e| e.to_str()),
            Some("cxx" | "cpp" | "cc" | "c")
        ) {
            build.file(&src);
            n += 1;
        }
    }

    if n == 0 {
        // Curation (Phase 0b/0c) not started yet. The crate is a
        // workspace member, so a bare `cargo build` reaches here
        // before vendor/ is populated — keep the tree green by
        // emitting an empty archive. The `extern "C"` ABI stays
        // undefined, which is harmless until the `fldigi` feature is
        // enabled and the block actually calls it.
        let stub = std::env::var("OUT_DIR").unwrap();
        let stub = Path::new(&stub).join("fldigi_stub.c");
        std::fs::write(&stub, "/* fldigi curation not started */\n").unwrap();
        cc::Build::new().file(&stub).compile("ferrite_fldigi");
        return;
    }

    build.compile("ferrite_fldigi");
}

/// Every immediate subdir of `vendor/` is an include root (fldigi uses
/// flat `#include "rtty.h"` / `#include "filters.h"` — its headers live
/// in `include/`, sources in per-mode dirs; we flatten on copy).
fn vendor_include_dirs(vendor: &Path) -> Vec<PathBuf> {
    let mut out = vec![vendor.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(vendor) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                out.push(e.path());
            }
        }
    }
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.is_file() {
            out.push(p);
        }
    }
    out
}
