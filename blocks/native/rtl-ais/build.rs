// Build script for the vendored aisdecoder.
//
// One C archive: vendor/aisdecoder.c + vendor/sounddecoder.c +
// vendor/lib/{filter,hmalloc,protodec,receiver}.c + shim/ais_shim.c.
// The vendor sources have the network/UDP/TCP-listener paths stripped
// (`#if 0`'d inline) — see VENDOR.md for the full deviation list.
//
// Compiles for the host's native target *and* `wasm32-unknown-unknown`
// so the `AisDemod` block runs browser-side like the other vendored
// decoders (multimon / rtl_433 / wsprd / ft8). The wasm recipe mirrors
// those: clang at wasm32, our `libc-stubs/` headers ahead of wasi-libc
// (so `#include <stdio.h>/<time.h>` see the minimal stubs), static link
// against wasi-libc `libc.a`/`libm.a`, wasm-ld DCEs the unreferenced
// syscall side. No per-crate libc shim is needed: the compiled surface
// has no `fopen` (so the wsprd preopen-discovery freeze does not apply),
// and every socket / getaddrinfo / setsockopt call lives inside the
// `#if 0`'d UDP-bridge block so no networking symbol is referenced. The
// live `time()` calls (decoder stats / NMEA timestamp) resolve through
// wasi-libc exactly as rtl_433's do. The lone `<pthread.h>` (one mutex
// around the message list) resolves to `shim/wasm/pthread.h`, a no-op
// stand-in — wasm32 here is single-threaded so the mutex folds away.
//
// Bindgen surfaces just the four library entry points: `ais_init`,
// `ais_push_audio`, `ais_drain`, `ais_reset`. The aisdecoder symbols
// the shim calls (init_ais_decoder, run_rtlais_decoder, …) stay
// internal to the C side — declared `extern` in the shim, never
// crossed into Rust.

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");
    // libc-stubs lives next to the other native crates and gives the
    // wasm32 target minimal <stdio.h>/<time.h> declarations without a
    // full host libc leaking through.
    let stubs_dir = manifest_dir.join("../libc-stubs");

    // --- 1. Compile the C library -----------------------------------------

    let mut build = cc::Build::new();
    build.include(&shim).include(&vendor).warnings(false);

    // Vendor uses old-style C; mute the warning classes that fire on it.
    build.flag_if_supported("-Wno-format-truncation");
    build.flag_if_supported("-Wno-unused-but-set-variable");
    build.flag_if_supported("-Wno-unused-variable");
    build.flag_if_supported("-Wno-unused-function");
    build.flag_if_supported("-Wno-incompatible-pointer-types");

    if target.starts_with("wasm32") {
        // Same recipe as ft8 / multimon-ng / rtl_433 / wsprd wasm paths:
        // clang at wasm32-unknown-unknown, drop the host libc include
        // search, layer our stubs ahead of wasi-libc so things that
        // #include <stdio.h>/<time.h> see the minimal declarations we
        // provide. wasi-libc supplies <math.h>/<stdlib.h>/<string.h>
        // behind. Static link against libc.a + libm.a via the link
        // directives below; wasm-ld DCEs everything unreferenced.
        build
            .compiler("clang")
            .flag("--target=wasm32-unknown-unknown")
            .flag("-nostdlibinc")
            .flag("-fno-builtin")
            // shim/wasm/ first so `<pthread.h>` resolves to our no-op
            // stand-in (native uses the real system header — this dir
            // is never on the native include path).
            .flag("-isystem")
            .flag(shim.join("wasm").to_str().unwrap())
            .flag("-isystem")
            .flag(stubs_dir.join("include").to_str().unwrap())
            .flag("-isystem")
            .flag("/usr/include/wasm32-wasi");
    }

    build.file(vendor.join("aisdecoder.c"));
    build.file(vendor.join("sounddecoder.c"));
    build.file(vendor.join("lib/filter.c"));
    build.file(vendor.join("lib/hmalloc.c"));
    build.file(vendor.join("lib/protodec.c"));
    build.file(vendor.join("lib/receiver.c"));
    build.file(shim.join("ais_shim.c"));
    build.compile("rtl_ais_vendor");

    if target.starts_with("wasm32") {
        // wasi-libc for mem*/strlen/snprintf (the NMEA assembly in
        // protodec.c must use the real sprintf — a no-op stub would
        // corrupt every !AIVDM sentence) and time(); libm for the
        // filter/receiver float math. Static link — wasm-ld only emits
        // referenced symbols, and the networking side is `#if 0`'d out.
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    println!("cargo:rerun-if-changed=vendor/aisdecoder.c");
    println!("cargo:rerun-if-changed=vendor/sounddecoder.c");
    println!("cargo:rerun-if-changed=vendor/lib/protodec.c");
    println!("cargo:rerun-if-changed=vendor/lib/receiver.c");
    println!("cargo:rerun-if-changed=vendor/lib/filter.c");
    println!("cargo:rerun-if-changed=vendor/lib/hmalloc.c");
    println!("cargo:rerun-if-changed=shim/ais_shim.c");
    println!("cargo:rerun-if-changed=shim/ais_shim.h");
    println!("cargo:rerun-if-changed=shim/wasm/pthread.h");

    // --- 2. Generate Rust bindings ---------------------------------------
    //
    // Force bindgen's libclang to parse with the host triple even when
    // cargo is building for wasm32 — same workaround multimon-ng /
    // rtl_433 / liquid-dsp use. Under `--target=wasm32-unknown-unknown`
    // libclang silently drops most function declarations (it parses
    // with no platform-default headers). The generated Rust bindings
    // are pure ABI shapes that match either compiled archive at link
    // time, so generating against the host is safe.
    let host = std::env::var("HOST")
        .expect("HOST set by cargo")
        .replace("riscv64gc-", "riscv64-");
    let bindings = bindgen::Builder::default()
        .header(shim.join("ais_shim.h").to_string_lossy())
        .clang_arg(format!("--target={host}"))
        .clang_arg(format!("-I{}", shim.display()))
        .allowlist_function("ais_init")
        .allowlist_function("ais_push_audio")
        .allowlist_function("ais_drain")
        .allowlist_function("ais_reset")
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen: generate rtl-ais bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("ais_bindings.rs"))
        .expect("bindgen: write rtl-ais bindings");

    println!("cargo:rerun-if-changed=build.rs");
}
