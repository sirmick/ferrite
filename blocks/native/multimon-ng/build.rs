// Build script for the vendored multimon-ng.
//
// Mirrors the per-decoder modular build pattern: we don't run upstream's
// CMake (it's geared at building the standalone CLI binary with stdin
// audio I/O — none of which is useful in a Rust-runtime context).
// Instead we cherry-pick just the demod sources we wrap, plus the BCH
// FEC and the per-decoder support files they need, and link them as a
// static archive against our shim that replaces unixinput.c's globals
// and `_verbprintf` with a thread-local capture buffer the Rust side
// drains.
//
// New decoder support is incremental: add the demod_<x>.c file (and
// any support .c it includes) to `decoder_sources()`, then add a
// `pub use sys::demod_<x>` re-export in `src/lib.rs` and a thin Rust
// `Block` impl in the parent `ferrite-blocks` crate.

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");
    let stubs_dir = manifest_dir.join("../libc-stubs");

    // --- 1. Compile the C library (decoder sources + shim) -----------------
    let mut build = cc::Build::new();
    build.include(&vendor).include(&shim).warnings(false);

    if target.starts_with("wasm32") {
        // Same recipe as liquid-dsp's wasm path: clang targeting bare
        // wasm32, our libc-stubs/ headers ahead of wasi-libc on the
        // include path so things that #include <stdio.h>/<time.h> see
        // the minimal stubs, and wasi-libc's full headers behind for
        // <string.h>, <stdlib.h>, <math.h>, etc. Linking against
        // wasi-libc's libc.a / libm.a happens via cargo:rustc-link-lib
        // below — a transitive consumer (the wasm-pack runtime crate)
        // picks it up.
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

    for src in decoder_sources(&vendor) {
        assert!(
            src.exists(),
            "multimon-ng source missing: {} — has the vendor tree been re-synced incorrectly?",
            src.display()
        );
        build.file(src);
    }
    build.file(shim.join("multimon_shim.c"));
    build.compile("multimon_vendor");

    if target.starts_with("wasm32") {
        // wasi-libc for memcpy/memset/strchr/printf/snprintf, plus
        // libm for sqrtf/cosf/sinf/expf/logf that the demods pull in
        // (DTMF Goertzel, EAS bit-timing math, BCH FEC). Static link
        // — wasm-ld only emits referenced symbols, so the syscall
        // side of libc stays out of the bundle as long as the demod
        // sources don't reach for it (and they don't, after our
        // shim neutralises printf / file I/O).
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    // --- 2. Generate Rust bindings via bindgen -----------------------------
    //
    // Allowlist is intentionally tight: we only need the per-decoder
    // `demod_<x>` extern statics, the `demod_param` and `demod_state`
    // structs, the `buffer_t` union, and our shim's drain helper.
    // Pulling everything from multimon.h would surface internal types
    // (cJSON, gen_*, the various l2_state_*) we don't bind to.
    //
    // Force bindgen's libclang to parse with the host triple even when
    // cargo is building for wasm32 — same workaround liquid-dsp uses,
    // and necessary for the same reason: under `--target=wasm32-…`
    // libclang silently drops most of the function declarations from
    // multimon's headers (a quirk of how it parses with no
    // platform-default headers available). The generated Rust bindings
    // are pure ABI shapes that match either compiled archive at link
    // time, so generating against the host is safe.
    let host = std::env::var("HOST")
        .expect("HOST set by cargo")
        .replace("riscv64gc-", "riscv64-");
    let bindings = bindgen::Builder::default()
        .header(vendor.join("multimon.h").to_string_lossy())
        .header(shim.join("multimon_shim.h").to_string_lossy())
        .clang_arg(format!("--target={host}"))
        .clang_arg(format!("-I{}", vendor.display()))
        .clang_arg(format!("-I{}", shim.display()))
        // Per-decoder externs we currently wrap:
        .allowlist_var("demod_poc5")
        .allowlist_var("demod_poc12")
        .allowlist_var("demod_poc24")
        .allowlist_var("demod_flex")
        .allowlist_var("demod_flex_next")
        .allowlist_var("demod_afsk1200")
        .allowlist_var("demod_afsk2400")
        .allowlist_var("demod_afsk2400_2")
        .allowlist_var("demod_afsk2400_3")
        .allowlist_var("demod_fsk9600")
        .allowlist_var("demod_morse")
        .allowlist_var("demod_eas")
        .allowlist_var("demod_dtmf")
        // POCSAG family runtime-tunable globals — wrapped via setter
        // functions in the shim because bindgen can't see globals
        // that are only defined in .c sources.
        .allowlist_function("multimon_pocsag_.*")
        // Common shapes:
        .allowlist_type("demod_param")
        .allowlist_type("demod_state")
        .allowlist_type("buffer_t")
        // Our shim:
        .allowlist_function("multimon_drain")
        .allowlist_function("multimon_reset_buffer")
        // Skip generating layout tests for opaque unions inside
        // demod_state — the union of l1/l2 state is huge and the
        // layout tests bindgen emits for them are ABI-fragile across
        // toolchain versions. We never inspect those fields from Rust.
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen: generate multimon-ng bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("multimon_bindings.rs"))
        .expect("bindgen: write bindings");

    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        shim.join("multimon_shim.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        shim.join("multimon_shim.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("multimon.h").display()
    );
}

/// Per-decoder source list. Phase 0 ships POCSAG1200 only; new decoders
/// add their `demod_<x>.c` + any per-decoder support file (e.g.
/// `pocsag.c` for the POCSAG family) here. The BCH FEC + stub are
/// shared infrastructure that come along for free.
fn decoder_sources(vendor: &std::path::Path) -> Vec<PathBuf> {
    let mut out = vec![
        // Shared BCH FEC + stub. Cheap; even decoders that don't use
        // BCH still link against the symbols indirectly.
        vendor.join("bch.c"),
        vendor.join("bch_stub.c"),
        // cJSON is unconditionally referenced by every decoder that
        // supports `json_mode` — we leave json_mode=0 in the shim, so
        // the JSON branches never execute, but the linker still wants
        // the symbols. Negligible code size.
        vendor.join("cJSON.c"),
    ];
    // POCSAG family — demod_poc5/12/24 wrap the 512/1200/2400 baud
    // receivers respectively. All three share `pocsag.c`, which
    // carries the codeword assembler + character decoders + the
    // `pocsag_*` config globals (mode, polarity, error correction,
    // show-partial) that callers tune. Wrapping all three is cheap
    // and lets a single PocsagDemod block try every rate in parallel
    // — paging carriers in the wild are a mix of all three.
    out.push(vendor.join("demod_poc5.c"));
    out.push(vendor.join("demod_poc12.c"));
    out.push(vendor.join("demod_poc24.c"));
    out.push(vendor.join("pocsag.c"));
    // FLEX family — demod_flex.c is the classic 1600/3200/6400 bps
    // 4-FSK paging mode that largely replaced POCSAG in the US in
    // the 2000s. demod_flex_next.c is a newer multimon-ng variant
    // with improved decoding. Both share the same 22050 Hz input
    // rate as POCSAG, so a paging block can run all five in
    // parallel on the same audio stream.
    out.push(vendor.join("demod_flex.c"));
    out.push(vendor.join("demod_flex_next.c"));
    // Packet family — AX.25 over AFSK1200 (the APRS workhorse) plus
    // its 2400 baud variants and the G3RUH 9600 baud FSK used on
    // amateur satellites. All produce HDLC frames decoded in the
    // shared `hdlc.c`. Same 22050 Hz contract as the paging family.
    out.push(vendor.join("demod_afsk12.c"));
    out.push(vendor.join("demod_afsk24.c"));
    out.push(vendor.join("demod_afsk24_2.c"));
    out.push(vendor.join("demod_afsk24_3.c"));
    out.push(vendor.join("demod_fsk96.c"));
    out.push(vendor.join("hdlc.c"));
    // Standalone audio-tone decoders, all 22050 Hz, no shared L2 layer.
    out.push(vendor.join("demod_morse.c"));
    out.push(vendor.join("demod_eas.c"));
    out.push(vendor.join("demod_dtmf.c"));
    // Pre-generated cosine LUT used via the `COS()` macro in
    // multimon.h. demod_dtmf is the only currently-wrapped consumer
    // (other future ones — CCIR/EEA/EIA/ZVEI selcall — would also
    // need it). One small data file; cheap to link unconditionally.
    out.push(vendor.join("costabf.c"));
    out
}
