// Build script for the vendored rtl_433.
//
// Same shape as the multimon-ng / dump1090 / ft8 / rtl-ais builds: pull
// in the vendor sources directly (skipping upstream's CMake, which is
// geared at building the standalone CLI), add our shim, link as a
// static archive. Native and WASM both — the wasm path threads through
// `../libc-stubs/include` + `/usr/include/wasm32-wasi` so stdio /
// stdlib calls left in the vendor sources resolve to lightweight stubs
// rather than full wasi-libc.
//
// Adding new vendor files: drop them in `vendor/src/` (or
// `vendor/src/devices/`) and they'll be picked up by the glob below.
// Trim list (what we don't compile) is documented in VENDOR.md.

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let vendor_src = vendor.join("src");
    let vendor_include = vendor.join("include");
    let shim = manifest_dir.join("shim");
    let stubs_dir = manifest_dir.join("../libc-stubs");

    // --- 1. Compile the C library (vendor sources + shim) -----------------

    let mut build = cc::Build::new();
    // shim/ first so our minimal `mongoose.h` (and any future overrides)
    // win against the vendor copy.
    build
        .include(&shim)
        .include(&vendor_include)
        .warnings(false);

    if target.starts_with("wasm32") {
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

    // Mute the warning classes upstream is loud on. Same set the other
    // vendor crates use.
    build.flag_if_supported("-Wno-format-truncation");
    build.flag_if_supported("-Wno-unused-but-set-variable");
    build.flag_if_supported("-Wno-unused-variable");
    build.flag_if_supported("-Wno-unused-function");
    build.flag_if_supported("-Wno-incompatible-pointer-types");
    build.flag_if_supported("-Wno-sign-compare");

    // Several vendor files (r_api.c at minimum) reference `errno` and
    // `UINT_MAX` without explicitly including <errno.h> / <limits.h>,
    // relying on glibc's transitive include from <stdio.h>. wasi-libc
    // is stricter and so are some gcc versions. Force-include both
    // ahead of every TU on both targets, matching the vendor-port-guide
    // "shim header" pattern (here we just borrow the system headers).
    build.flag("-include").flag("errno.h");
    build.flag("-include").flag("limits.h");

    // Glob the vendor core + every device decoder.
    for src in vendor_sources(&vendor_src) {
        assert!(
            src.exists(),
            "rtl_433 source missing: {} — has the vendor tree been resynced?",
            src.display()
        );
        build.file(src);
    }

    // Shim (added once the file exists; cc::Build doesn't mind an empty
    // archive otherwise — see VENDOR.md). Wrap in cfg to keep cargo
    // check working before the shim lands.
    let shim_c = shim.join("rtl433_shim.c");
    if shim_c.exists() {
        build.file(shim_c);
    }

    build.compile("rtl_433_vendor");

    if target.starts_with("wasm32") {
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    println!("cargo:rerun-if-changed=vendor/src");
    println!("cargo:rerun-if-changed=vendor/include");
    println!("cargo:rerun-if-changed=shim");
    println!("cargo:rerun-if-changed=build.rs");

    // --- 2. Generate Rust bindings ---------------------------------------
    //
    // Tight allowlist: only the five shim ABI functions and the opaque
    // state struct. Internal vendor types stay invisible to Rust.
    //
    // Force bindgen's libclang to parse with the HOST triple even when
    // cargo is building for wasm32 — same workaround as multimon-ng
    // and liquid-dsp use. Under `--target=wasm32-unknown-unknown`
    // libclang silently fails to surface function declarations whose
    // arguments mention `size_t` (stddef.h's stddef.h-from-no-platform
    // is missing the typedef). The generated Rust bindings are pure
    // ABI shapes that match either compiled archive at link time, so
    // generating against the host is safe.
    let host = std::env::var("HOST")
        .expect("HOST set by cargo")
        .replace("riscv64gc-", "riscv64-");
    let bindings = bindgen::Builder::default()
        .header(shim.join("rtl433_shim.h").to_string_lossy())
        .clang_arg(format!("--target={host}"))
        .clang_arg(format!("-I{}", shim.display()))
        .allowlist_function("rtl433_init")
        .allowlist_function("rtl433_free")
        .allowlist_function("rtl433_reset")
        .allowlist_function("rtl433_push_iq")
        .allowlist_function("rtl433_drain_event")
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen: generate rtl_433 bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("rtl433_bindings.rs"))
        .expect("bindgen: write rtl_433 bindings");
}

fn vendor_sources(vendor_src: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_c_files(vendor_src, &mut out);
    out
}

fn collect_c_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_c_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "c") {
            out.push(path);
        }
    }
}
