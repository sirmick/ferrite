//! Native: compile vendored mbelib + the shim with `cc` and link
//! statically. wasm32: compile nothing — the shim ABI in `src/lib.rs`
//! is a wasm-bindgen import satisfied by the sibling Emscripten module
//! built by `emscripten/build.sh` (same link-vs-bridge split as
//! `ferrite-fldigi`; mbelib needs libm, which `wasm32-unknown-unknown`
//! doesn't have).
use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET");
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
    for sub in ["vendor", "shim"] {
        for e in std::fs::read_dir(dir.join(sub)).unwrap().flatten() {
            println!("cargo:rerun-if-changed={}", e.path().display());
        }
    }
    if target.starts_with("wasm32") {
        return;
    }
    let mut b = cc::Build::new();
    b.include(dir.join("vendor")).warnings(false).opt_level(2);
    for f in [
        "mbelib.c",
        "ecc.c",
        "ambe3600x2400.c",
        "ambe3600x2450.c",
        "imbe7200x4400.c",
        "imbe7100x4400.c",
    ] {
        b.file(dir.join("vendor").join(f));
    }
    b.file(dir.join("shim/mbe_shim.c"));
    b.compile("ferrite_mbe");
    if !target.contains("windows") {
        println!("cargo:rustc-link-lib=m");
    }
}
