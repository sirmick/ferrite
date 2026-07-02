//! Build script for the native whisper.cpp speech-to-text core.
//!
//! ## Two backends, one C ABI (the fldigi pattern)
//!
//! The `wsp_init` / `wsp_transcribe` / `wsp_vad_available` C ABI in
//! `shim/whisper_glue.c` is the single seam. How it's satisfied depends
//! on the target:
//!
//! * **Native** (`cargo build`): build whisper.cpp's own CMake to get
//!   static `libwhisper.a` + `libggml*.a` (exactly what the emscripten
//!   script does, minus `emcc`), compile the C glue against them, and
//!   statically link. `ld` resolves the ABI. This is the wsprd/fldigi
//!   native pattern.
//!
//! * **wasm32** (`wasm-pack`): compile *nothing* here. The browser does
//!   not link this crate — it loads the sibling Emscripten `whisper.mjs`
//!   module directly from the transcribe Web Worker. So the wasm build
//!   of this crate is a pure stub (see `src/lib.rs`), and this script
//!   returns early.
//!
//! ## Inert until vendored
//!
//! Like the fldigi/whisper emscripten scripts: if `vendor/whisper.cpp`
//! isn't present, emit an empty archive so a bare `cargo build` of the
//! workspace stays green. The `wsp_*` symbols stay undefined, which is
//! harmless until something actually calls the engine.

use std::path::{Path, PathBuf};

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = crate_dir.join("vendor/whisper.cpp");
    let glue = crate_dir.join("shim/whisper_glue.c");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", glue.display());
    println!("cargo:rerun-if-changed={}", vendor.display());

    // Browser path: this crate is a stub on wasm32 (the real engine is
    // the Emscripten module loaded by the TS worker). Compile nothing.
    if target.starts_with("wasm32") {
        return;
    }

    // Inert until whisper.cpp is vendored — keep the workspace green.
    if !vendor.join("include/whisper.h").exists() {
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let stub = out.join("whisper_stub.c");
        std::fs::write(&stub, "/* whisper.cpp not vendored */\n").unwrap();
        cc::Build::new().file(&stub).compile("ferrite_whisper_glue");
        println!(
            "cargo:warning=ferrite-whisper: vendor/whisper.cpp absent — built stub. \
             Clone it (see blocks/native/whisper/emscripten/build.sh) to enable native STT."
        );
        return;
    }

    // 1. Build whisper.cpp's static libs via its own CMake. Mirror the
    //    emscripten script's flags: CPU backend only, no examples/tests/
    //    server, no OpenMP. `cmake` crate installs into OUT_DIR.
    let dst = cmake::Config::new(&vendor)
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("WHISPER_BUILD_EXAMPLES", "OFF")
        .define("WHISPER_BUILD_TESTS", "OFF")
        .define("WHISPER_BUILD_SERVER", "OFF")
        .define("GGML_OPENMP", "OFF")
        .define("CMAKE_BUILD_TYPE", "Release")
        .build_target("whisper")
        .build();

    // 2. Tell the linker where the freshly-built archives live. whisper.cpp
    //    scatters them across build/src and build/ggml/src; add every dir
    //    that contains a static lib so a layout bump doesn't break us.
    for dir in find_lib_dirs(&dst.join("build")) {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    // Link order matters: whisper → ggml → ggml-cpu/base. The static
    // libs present vary by whisper.cpp version, so link whatever exists.
    for lib in detect_libs(&dst.join("build")) {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    // whisper.cpp is C++ — pull in the standard library.
    link_cpp_stdlib(&target);

    // 3. Compile our C glue against whisper's headers and let it resolve
    //    the wsp_* symbols at the final Rust link.
    cc::Build::new()
        .file(&glue)
        .include(vendor.join("include"))
        .include(vendor.join("ggml/include"))
        .flag_if_supported("-O3")
        .warnings(false)
        .compile("ferrite_whisper_glue");
}

/// Every dir under `build` that holds a `lib*.a`, deduped.
fn find_lib_dirs(build: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = walk(build)
        .into_iter()
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("a")
                && p.file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f.starts_with("lib"))
        })
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Static lib basenames (no `lib` prefix / `.a` suffix) found under
/// `build`, ordered whisper-first so the static link resolves.
fn detect_libs(build: &Path) -> Vec<String> {
    let mut libs: Vec<String> = walk(build)
        .into_iter()
        .filter_map(|p| {
            let f = p.file_name()?.to_str()?;
            (f.starts_with("lib") && f.ends_with(".a")).then(|| {
                f.trim_start_matches("lib")
                    .trim_end_matches(".a")
                    .to_string()
            })
        })
        .collect();
    libs.sort();
    libs.dedup();
    // Dependents before dependencies for static linking.
    libs.sort_by_key(|l| match l.as_str() {
        "whisper" => 0,
        "ggml" => 1,
        "ggml-cpu" => 2,
        "ggml-base" => 3,
        _ => 4,
    });
    libs
}

fn link_cpp_stdlib(target: &str) {
    if target.contains("apple") || target.contains("freebsd") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
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
        } else {
            out.push(p);
        }
    }
    out
}
