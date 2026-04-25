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
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");

    // --- 1. Compile the C library (decoder sources + shim) -----------------
    let mut build = cc::Build::new();
    build.include(&vendor).include(&shim).warnings(false);

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

    // --- 2. Generate Rust bindings via bindgen -----------------------------
    //
    // Allowlist is intentionally tight: we only need the per-decoder
    // `demod_<x>` extern statics, the `demod_param` and `demod_state`
    // structs, the `buffer_t` union, and our shim's drain helper.
    // Pulling everything from multimon.h would surface internal types
    // (cJSON, gen_*, the various l2_state_*) we don't bind to.
    let bindings = bindgen::Builder::default()
        .header(vendor.join("multimon.h").to_string_lossy())
        .header(shim.join("multimon_shim.h").to_string_lossy())
        .clang_arg(format!("-I{}", vendor.display()))
        .clang_arg(format!("-I{}", shim.display()))
        // Per-decoder externs we currently wrap:
        .allowlist_var("demod_poc12")
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
    // POCSAG family — demod_poc12 wraps the 1200 baud receiver; the
    // common pocsag.c carries the codeword assembler + character
    // decoders + the `pocsag_*` config globals that callers tune.
    out.push(vendor.join("demod_poc12.c"));
    out.push(vendor.join("pocsag.c"));
    out
}
