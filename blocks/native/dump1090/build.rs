// Build script for the vendored dump1090 (antirez classic).
//
// One C archive: vendor/dump1090.c (with the IO/network/main paths
// `#if 0`'d out — see the file's preamble) + shim/dump1090_shim.c. The
// vendor source `#include`s our shim header for the type stubs and the
// `printf` capture macro; the shim source `#include`s the same header
// for its declarations and reaches into the Modes global via accessor
// helpers appended at the bottom of dump1090.c.
//
// Bindgen surfaces just the four library entry points:
// `dump1090_init`, `dump1090_push_iq_u8`, `dump1090_drain`,
// `dump1090_reset`. Nothing from the C decode core leaks into Rust —
// the wrapper only hands across u8 IQ in and text-line bytes out, same
// envelope as the multimon-ng wrapper.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let shim = manifest_dir.join("shim");

    // --- 1. Compile the C library -----------------------------------------

    let mut build = cc::Build::new();
    build.include(&shim).include(&vendor).warnings(false);

    // dump1090.c uses old-style `void f()` (= variadic in C89 strict);
    // since we don't care about strict checking on vendored code, mute
    // the warning class entirely.
    build.flag_if_supported("-Wno-format-truncation");
    build.flag_if_supported("-Wno-unused-but-set-variable");
    build.flag_if_supported("-Wno-unused-variable");
    build.flag_if_supported("-Wno-unused-function");
    build.flag_if_supported("-Wno-incompatible-pointer-types");

    build.file(vendor.join("dump1090.c"));
    build.file(shim.join("dump1090_shim.c"));
    build.compile("dump1090_vendor");

    println!("cargo:rerun-if-changed=vendor/dump1090.c");
    println!("cargo:rerun-if-changed=shim/dump1090_shim.c");
    println!("cargo:rerun-if-changed=shim/dump1090_shim.h");

    // --- 2. Generate Rust bindings ---------------------------------------
    //
    // Tight allowlist: only the four library entry points the Rust
    // wrapper calls. Modes' internals stay private to the C side.
    let bindings = bindgen::Builder::default()
        .header(shim.join("dump1090_shim.h").to_string_lossy())
        .clang_arg(format!("-I{}", shim.display()))
        .allowlist_function("dump1090_init")
        .allowlist_function("dump1090_push_iq_u8")
        .allowlist_function("dump1090_drain")
        .allowlist_function("dump1090_reset")
        .allowlist_function("dump1090_aircraft_snapshot")
        .allowlist_type("ferrite_aircraft")
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen: generate dump1090 bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("dump1090_bindings.rs"))
        .expect("bindgen: write dump1090 bindings");

    println!("cargo:rerun-if-changed=build.rs");
}
