// Build script for the vendored aisdecoder.
//
// One C archive: vendor/aisdecoder.c + vendor/sounddecoder.c +
// vendor/lib/{filter,hmalloc,protodec,receiver}.c + shim/ais_shim.c.
// The vendor sources have the network/UDP/TCP-listener paths stripped
// (`#if 0`'d inline) — see VENDOR.md for the full deviation list.
//
// Bindgen surfaces just the four library entry points: `ais_init`,
// `ais_push_audio`, `ais_drain`, `ais_reset`. The aisdecoder symbols
// the shim calls (init_ais_decoder, run_rtlais_decoder, …) stay
// internal to the C side — declared `extern` in the shim, never
// crossed into Rust.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");

    // --- 1. Compile the C library -----------------------------------------

    let mut build = cc::Build::new();
    build.include(&shim).include(&vendor).warnings(false);

    // Vendor uses old-style C; mute the warning classes that fire on it.
    build.flag_if_supported("-Wno-format-truncation");
    build.flag_if_supported("-Wno-unused-but-set-variable");
    build.flag_if_supported("-Wno-unused-variable");
    build.flag_if_supported("-Wno-unused-function");
    build.flag_if_supported("-Wno-incompatible-pointer-types");

    build.file(vendor.join("aisdecoder.c"));
    build.file(vendor.join("sounddecoder.c"));
    build.file(vendor.join("lib/filter.c"));
    build.file(vendor.join("lib/hmalloc.c"));
    build.file(vendor.join("lib/protodec.c"));
    build.file(vendor.join("lib/receiver.c"));
    build.file(shim.join("ais_shim.c"));
    build.compile("rtl_ais_vendor");

    println!("cargo:rerun-if-changed=vendor/aisdecoder.c");
    println!("cargo:rerun-if-changed=vendor/sounddecoder.c");
    println!("cargo:rerun-if-changed=vendor/lib/protodec.c");
    println!("cargo:rerun-if-changed=vendor/lib/receiver.c");
    println!("cargo:rerun-if-changed=vendor/lib/filter.c");
    println!("cargo:rerun-if-changed=vendor/lib/hmalloc.c");
    println!("cargo:rerun-if-changed=shim/ais_shim.c");
    println!("cargo:rerun-if-changed=shim/ais_shim.h");

    // --- 2. Generate Rust bindings ---------------------------------------

    let bindings = bindgen::Builder::default()
        .header(shim.join("ais_shim.h").to_string_lossy())
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
