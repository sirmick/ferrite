// Build script for the vendored liquid-dsp.
//
// Mirrors the per-module `add_library(... OBJECT ...)` declarations
// from upstream's CMakeLists.txt — see `vendor/CMakeLists.txt` snapshot
// in the upstream repo at the pinned commit listed in `VENDOR.md`. We
// don't run upstream's CMake/autoconf because they assume a host libc
// and a working pkg-config; cc-rs gives us simpler cross-compile.
//
// Module-by-module philosophy:
// - Build only what we plan to expose through the Rust wrapper layer.
//   `agc`, `buffer`, `core`, `dotprod`, `filter`, `math`, `nco`,
//   `utility`, `vector` is the substrate everything DSP-side needs.
//   The bigger fish (`fec`, `framing`, `modem`, `multichannel`) get
//   added in their own milestones once we have wrappers for them.
// - Skip every module that touches concerns we don't care about today
//   (`audio` cvsd, `channel` modeling, `equalization`, `fft` — we use
//   rustfft —, `matrix`, `optim`, `quantization`, `random` upstream's
//   PRNGs, `sequence`).
// - Cost: extra modules cost compile time, not runtime. Once a module
//   is needed, add it here; nothing else changes.
//
// Two compile targets, one cc-rs invocation each:
// - native: cc-rs picks gcc/clang itself, system libm/libc reachable.
// - wasm32-unknown-unknown: clang `--target=wasm32-unknown-unknown`,
//   `-nostdlibinc -fno-builtin`, with our `blocks/native/libc-stubs/`
//   shim ahead of wasi-libc on the include path. See M1's
//   `blocks/native/README.md` for why the libc-stubs interpose is
//   necessary (wasi-libc gates `<wasi/api.h>` on the wasm32-wasi ABI).

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor");
    let stubs_dir = manifest_dir.join("../libc-stubs");

    // --- 1. Compile the C library ---------------------------------------------------------------
    let mut build = cc::Build::new();
    build
        .include(vendor.join("include"))
        // liquid.internal.h includes module-private headers like
        // <bench/banner.h> and <utility/bench/banner.h> — but those
        // are only enabled by autotest paths we don't compile. The
        // top-level vendor include path is enough.
        .include(&vendor)
        .warnings(false);

    if target.starts_with("wasm32") {
        build
            .compiler("clang")
            .flag("--target=wasm32-unknown-unknown")
            .flag("-nostdlibinc")
            .flag("-fno-builtin")
            // libc-stubs first so our stdio.h / time.h interposes
            // ahead of wasi-libc's, which gates on the WASI ABI.
            .flag("-isystem")
            .flag(stubs_dir.join("include").to_str().unwrap())
            .flag("-isystem")
            .flag("/usr/include/wasm32-wasi");
    }

    for src in liquid_sources(&vendor) {
        build.file(src);
    }
    // libc-stubs/stubs.c is intentionally NOT linked here — once we
    // link wasi-libc's libc.a (below) for malloc/free/cexpf/etc.,
    // libc owns the stdio symbols (`printf`, `stderr`, …) and a
    // duplicate definition from our stubs is a hard link error. The
    // libc-stubs/include/ headers are still in front of wasi-libc's
    // include path at *compile* time so liquid's `#include <stdio.h>`
    // resolves through our minimal types instead of wasi's, but the
    // symbols those declarations refer to come from real libc on link.

    build.compile("liquid_vendor");

    if target.starts_with("wasm32") {
        // wasi-libc's libm + libc. libm covers sqrtf/cosf/sinf/expf
        // (real math); libc covers malloc/free + the complex helpers
        // like cexpf which liquid pulls in for windowing math. The
        // link is fine-grained — wasm-ld only emits objects whose
        // symbols are referenced, so nothing from libc's syscall side
        // (open/read/clock_gettime/…) lands in the bundle as long as
        // liquid doesn't call those.
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
        println!("cargo:rustc-link-lib=static=c");
    }

    // --- 2. Generate Rust bindings via bindgen --------------------------------------------------
    //
    // Always generate against the host clang's headers, regardless of
    // target. Why: bindgen on `--target=wasm32-unknown-unknown`
    // silently drops most function declarations after liquid_float_
    // complex's typedef — likely a libclang quirk parsing the
    // macro-expanded `LIQUID_FIRFILT_DEFINE_API(...)` blob under the
    // wasm target. The generated Rust bindings are pure ABI shapes
    // (int/float/pointer/struct), identical between native and WASM
    // for the C functions we expose, so generating once for the host
    // and linking against either compiled liquid archive is correct.
    //
    // Allowlist what we actually wrap so we don't pull every type in
    // liquid.h into the bindings (~10k LOC). Each module that grows a
    // safe wrapper appends its types/functions here.
    let bindings = bindgen::Builder::default()
        .header(vendor.join("include/liquid.h").to_string_lossy())
        // Force the host triple — see comment above. Keeps bindgen's
        // libclang from picking up the wasm32 target wrt cargo's
        // TARGET env var, which would steer it at headers (or a lack
        // of them) inappropriate for what we're parsing.
        .clang_arg("--target=x86_64-unknown-linux-gnu")
        .clang_arg(format!("-I{}", vendor.join("include").display()))
        // M2 surface — keep this list tight. New wrappers append.
        .allowlist_function("firfilt_rrrf_.*")
        .allowlist_function("ampmodem_.*")
        .allowlist_function("liquid_(?:libversion|error_str)")
        .allowlist_type("firfilt_rrrf")
        .allowlist_type("ampmodem")
        .allowlist_type("liquid_ampmodem_type")
        .allowlist_type("liquid_float_complex")
        .allowlist_var("LIQUID_.*")
        // Emit cargo:rerun-if-changed for every header bindgen sees.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen: generate liquid bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("liquid_bindings.rs"))
        .expect("bindgen: write bindings");

    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("include/liquid.h").display()
    );
}

/// Per-module source list, mirroring upstream `CMakeLists.txt`'s
/// `add_library(... OBJECT ...)` declarations at the pinned commit. If
/// upstream rearranges files in a future bump, walk the new
/// CMakeLists.txt and resync this list — keeping it explicit beats
/// glob-ing the tree because we want the build to fail loudly on a
/// missing file rather than silently grow on an added one.
fn liquid_sources(vendor: &std::path::Path) -> Vec<PathBuf> {
    let modules: &[(&str, &[&str])] = &[
        ("agc", &["agc_crcf.c", "agc_rrrf.c"]),
        ("buffer", &["bufferf.c", "buffercf.c"]),
        ("core", &["logging.c", "error.c"]),
        // Portable C dotprod — no SIMD on this build. CMake picks SSE/
        // AVX/NEON by host capability; we trade that perf for a single
        // source list across native and WASM.
        (
            "dotprod",
            &[
                "dotprod_cccf.c",
                "dotprod_crcf.c",
                "dotprod_rrrf.c",
                "sumsq.c",
            ],
        ),
        (
            "filter",
            &[
                "bessel.c",
                "butter.c",
                "cheby1.c",
                "cheby2.c",
                "ellip.c",
                "filter_rrrf.c",
                "filter_crcf.c",
                "filter_cccf.c",
                "firdes.c",
                "firdespm.c",
                "firdespm_halfband.c",
                "fnyquist.c",
                "gmsk.c",
                "group_delay.c",
                "hM3.c",
                "iirdes.pll.c",
                "iirdes.c",
                "lpc.c",
                "rcos.c",
                "rkaiser.c",
                "rrcos.c",
            ],
        ),
        (
            "math",
            &[
                "poly.c",
                "polyc.c",
                "polyf.c",
                "polycf.c",
                "math.c",
                "math.bessel.c",
                "math.gamma.c",
                "math.complex.c",
                "math.trig.c",
                "modular_arithmetic.c",
                "poly.findroots.c",
                "windows.c",
            ],
        ),
        // We pull in just the AM/SSB demod entry from the modem
        // module (`ampmodem.c`); skipping `modemcf.c`, `fskdem.c`,
        // and the digital-modem zoo keeps the link small. The two
        // `_const.c` files are pre-generated symbol tables liquid
        // expects to find linked even when the digital paths aren't
        // exercised.
        ("modem", &["ampmodem.c", "modem.shim.c"]),
        ("nco", &["nco_crcf.c", "nco.utilities.c"]),
        (
            "utility",
            &[
                "bshift_array.c",
                "byte_utilities.c",
                "count_ones.c",
                "memory.c",
                "msb_index.c",
                "pack_bytes.c",
                "shift_array.c",
                "utility.c",
            ],
        ),
        ("vector", &["vectorf.port.c", "vectorcf.port.c"]),
    ];

    let mut out = Vec::new();
    for (module, files) in modules {
        let dir = vendor.join("src").join(module).join("src");
        for f in *files {
            let path = dir.join(f);
            assert!(
                path.exists(),
                "liquid source missing: {} — has upstream restructured? sync `liquid_sources()` against vendor/CMakeLists.txt",
                path.display()
            );
            out.push(path);
        }
    }
    out
}
