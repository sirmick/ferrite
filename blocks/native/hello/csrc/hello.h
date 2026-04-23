/* Smallest possible C → Rust ABI to validate the blocks/native/ build
   pipeline. Two functions, zero libc deps. If both compile and call
   correctly under cargo build (native) AND wasm-pack build (browser),
   the substrate is ready for real C vendors (liquid-dsp, multimon-ng).

   Keep this file zero-deps: no stdio, no malloc, no time. Adding any
   of those moves into wasi-libc territory, which is the next milestone. */

#ifndef FERRITE_HELLO_H
#define FERRITE_HELLO_H

/* Returns a + b. Trivial proof we can call C from Rust on both targets. */
int hello_add(int a, int b);

/* Sum a slice of f32 samples. A microcosm of every DSP block: the Rust
   side passes a pointer + length, the C side reads the samples. If this
   works the calling convention is OK. */
float hello_sumf(const float *samples, int n);

/* RMS of a slice — exercises libm (`sqrtf`). The point is to prove that
   linking math symbols works on both native and WASM. Native uses the
   system libm; WASM uses wasi-libc's libm (or a shim). If this compiles
   and runs, the substrate is ready for liquid-dsp's heavy math. */
float hello_rmsf(const float *samples, int n);

#endif
