// Build script for the hello vendor crate — the M1 substrate proof.
//
// Compiles `csrc/hello.c` for whichever target Cargo is asking for.
// Native (x86_64-linux, aarch64-darwin, …) gets the host's `cc`; the
// `wasm32-unknown-unknown` target gets clang with `--target=wasm32-…`
// and `-nostdlib` (no libc dep here — this file is the proof we can
// build C without any libc). Subsequent crates (liquid-dsp,
// multimon-ng) will lift this same pattern and add wasi-libc on top.

fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo");
    let mut build = cc::Build::new();
    build.file("csrc/hello.c").include("csrc").warnings(false);

    if target.starts_with("wasm32") {
        build
            .compiler("clang")
            .flag("--target=wasm32-unknown-unknown")
            // Don't pull in any host libc — vendored C uses the
            // wasi-libc headers via `-isystem` instead. `-fno-builtin`
            // keeps clang from synthesising calls into nominal libc
            // helpers that we don't link.
            .flag("-nostdlibinc")
            .flag("-fno-builtin")
            // Ubuntu's wasi-libc package puts headers under
            // /usr/include/wasm32-wasi. The headers are usable from
            // wasm32-unknown-unknown for the no-syscall portions
            // (libm in particular), which is what liquid-dsp will
            // reach for. If a future vendor pulls in syscalls
            // (file/socket/clock) we'll switch to wasi-sdk and the
            // matching wasm32-wasi/wasm32-wasip1 target.
            .flag("-isystem")
            .flag("/usr/include/wasm32-wasi");
    }

    build.compile("hello_vendor");

    // Link libm from wasi-libc on WASM. Native picks up libm
    // automatically through the system linker.
    if target.starts_with("wasm32") {
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=m");
    }

    println!("cargo:rerun-if-changed=csrc/hello.c");
    println!("cargo:rerun-if-changed=csrc/hello.h");
}
