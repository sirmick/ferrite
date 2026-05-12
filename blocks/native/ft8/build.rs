//! Build script for the vendored `ft8_lib`.
//!
//! Compiles a focused subset of `kgoba/ft8_lib` (FT8 + FT4 streaming
//! decode) for the host's native target *and* `wasm32-unknown-unknown`.
//! The wasm path mirrors `blocks/native/multimon-ng/build.rs`: clang
//! targeting bare wasm32, our `libc-stubs/` headers ahead of wasi-libc
//! on the include path, and a static link against wasi-libc's `libc.a`
//! / `libm.a` at consumer-bundling time.
//!
//! New file mapped: add it to `c_sources()` and ensure all upstream
//! includes resolve through the directories declared on the `cc::Build`
//! include path (`vendor/ft8`, `vendor/common`, `vendor/fft`, `shim`).

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");
    // The `libc-stubs/` header forest lives a level up next to the
    // multimon-ng + liquid-dsp crates and gives us minimal `<stdio.h>`
    // / `<time.h>` declarations the wasm32 target needs without a
    // full host libc poking through.
    let stubs_dir = manifest_dir.join("../libc-stubs");

    let mut build = cc::Build::new();
    build
        .include(&vendor)
        .include(vendor.join("ft8"))
        .include(vendor.join("common"))
        .include(vendor.join("fft"))
        .include(&shim)
        .warnings(false);

    if target.starts_with("wasm32") {
        // Same recipe as multimon-ng / liquid-dsp wasm paths: clang
        // pointing at `wasm32-unknown-unknown`, drop the host's
        // default libc include search, layer our stubs ahead of
        // wasi-libc so things that #include <stdio.h>/<time.h> see
        // the minimal declarations we provide. wasi-libc supplies
        // <math.h>/<stdlib.h>/<string.h> behind. Static link against
        // libc.a + libm.a happens via cargo:rustc-link-lib below;
        // wasm-ld DCEs everything not actually referenced.
        build
            .compiler("clang")
            .flag("--target=wasm32-unknown-unknown")
            .flag("-nostdlibinc")
            .flag("-fno-builtin")
            .flag("-isystem")
            .flag(stubs_dir.join("include").to_str().unwrap())
            .flag("-isystem")
            .flag("/usr/include/wasm32-wasi");
    }
    // Suppress LOG() output from the decoder. ft8_lib's
    // `vendor/ft8/debug.h` makes the LOG macro a no-op when
    // `LOG_LEVEL` is *undefined* (the `#ifdef LOG_LEVEL` /
    // `#else` branch). Defining it at any value enables the
    // macro — counter-intuitively `=0` would print everything
    // because the gate is `level >= LOG_LEVEL`. So we leave it
    // unset entirely. (Comment for the next person who hits this.)

    for src in c_sources(&vendor) {
        assert!(
            src.exists(),
            "ft8_lib source missing: {} — has the vendor tree been re-synced incorrectly?",
            src.display()
        );
        build.file(src);
    }
    // Shim glue: thin malloc/free wrapper around `monitor_t` plus
    // accessors for fields the FT8 decode path needs.
    build.file(shim.join("glue.c"));

    build.compile("ferrite_ft8");

    if target.starts_with("wasm32") {
        // ft8_lib pulls in libm (sinf/cosf/log10f/sqrtf/expf — Costas
        // sync, kiss_fft windowing, LDPC log-likelihood) and libc
        // (malloc/free/memset/memcpy from monitor.c + kiss_fft). Both
        // ride wasi-libc statically; wasm-ld emits only the symbols
        // ft8 actually pulls.
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    println!("cargo:rerun-if-changed=build.rs");
    for path in c_sources(&vendor) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed={}", shim.join("glue.c").display());
    for dir in ["ft8", "common", "fft"] {
        for entry in walk(&vendor.join(dir)) {
            if entry.extension().is_some_and(|e| e == "h") {
                println!("cargo:rerun-if-changed={}", entry.display());
            }
        }
    }
}

fn c_sources(vendor: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // ft8/ — encode + decode + LDPC + CRC + message + text.
    for f in [
        "constants.c",
        "crc.c",
        "decode.c",
        "encode.c",
        "ldpc.c",
        "message.c",
        "text.c",
    ] {
        out.push(vendor.join("ft8").join(f));
    }
    // common/ — monitor only; audio.c + wave.c are out of scope.
    out.push(vendor.join("common").join("monitor.c"));
    // fft/ — kiss_fft for the monitor's STFT.
    for f in ["kiss_fft.c", "kiss_fftr.c"] {
        out.push(vendor.join("fft").join(f));
    }
    out
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            out.push(p);
        }
    }
    out
}
