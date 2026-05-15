//! Build script for the vendored WSPR decoder (`wsprd`).
//!
//! Compiles the rtlsdr-wsprd extraction of WSJT-X's WSPR decode core
//! for the host's native target *and* `wasm32-unknown-unknown`, so the
//! `WsprDemod` block is `Placement::Either` like `Ft8Demod`.
//!
//! Upstream wsprd depends on FFTW3, which does not cross-compile to
//! bare wasm32. `shim/fftw3.h` maps the small `fftwf_*` surface wsprd
//! touches (one 512-pt forward complex FFT) onto the `kiss_fft`
//! vendored under `vendor/fft/` — the same FFT ft8_lib uses here. The
//! shim dir is first on the include path so `<fftw3.h>` resolves to it
//! rather than any system FFTW. The wasm recipe mirrors
//! `blocks/native/ft8/build.rs`: clang at wasm32, our `libc-stubs/`
//! ahead of wasi-libc, static link against wasi-libc `libc.a`/`libm.a`.
//!
//! New file mapped: add it to `c_sources()` and make sure its includes
//! resolve through `vendor/`, `vendor/fft/`, or `shim/`.

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

    let mut build = cc::Build::new();
    build
        // `shim` first: `<fftw3.h>` must resolve to our kiss_fft shim,
        // not a system FFTW that wouldn't link on wasm anyway.
        .include(&shim)
        .include(&vendor)
        .include(vendor.join("fft"))
        .warnings(false);

    if target.starts_with("wasm32") {
        // Same recipe as ft8 / multimon-ng / liquid-dsp wasm paths:
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
            .flag("-isystem")
            .flag(stubs_dir.join("include").to_str().unwrap())
            .flag("-isystem")
            .flag("/usr/include/wasm32-wasi");
    }

    for src in c_sources(&vendor) {
        assert!(
            src.exists(),
            "wsprd source missing: {} — has the vendor tree been re-synced incorrectly?",
            src.display()
        );
        build.file(src);
    }

    build.compile("ferrite_wsprd");

    if target.starts_with("wasm32") {
        // wsprd pulls libm (sinf/cosf/logf/powf/atan2f — sync,
        // kiss_fft, Fano metrics) and libc (malloc/free/mem*/snprintf
        // for the decoded callsign/grid formatting; fopen returns NULL
        // under wasm so the hashtable/wisdom guards short-circuit).
        // Both ride wasi-libc statically.
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", shim.join("fftw3.h").display());
    for path in c_sources(&vendor) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for entry in walk(&vendor) {
        if entry.extension().is_some_and(|e| e == "h") {
            println!("cargo:rerun-if-changed={}", entry.display());
        }
    }
    for entry in walk(&vendor.join("fft")) {
        if entry.extension().is_some_and(|e| e == "h") {
            println!("cargo:rerun-if-changed={}", entry.display());
        }
    }
}

fn c_sources(vendor: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // WSPR decode core: detector/demod + Fano + callsign hash +
    // tables + pack/unpack utils + symbol regen (signal subtraction).
    for f in [
        "wsprd.c",
        "fano.c",
        "nhash.c",
        "tab.c",
        "wsprd_utils.c",
        "wsprsim_utils.c",
    ] {
        out.push(vendor.join(f));
    }
    // kiss_fft backing the FFTW shim (vendor/fft/kiss_fft.c).
    out.push(vendor.join("fft").join("kiss_fft.c"));
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
