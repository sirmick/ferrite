/* ferrite: FFTW3 → kiss_fft shim.
 *
 * Vendored wsprd.c uses exactly one 512-point forward complex FFT — a
 * single plan created once and executed in a loop (the coarse
 * sync/spectrum stage). Everything else in the WSPR decode path
 * (Fano convolutional decode, callsign hash) is FFT-free.
 *
 * Upstream wsprd depends on FFTW3, which does not cross-compile to
 * `wasm32-unknown-unknown` the way the rest of blocks/native/* does.
 * This header maps the small `fftwf_*` surface wsprd touches onto the
 * `kiss_fft` already vendored under `vendor/fft/` (the same FFT
 * ft8_lib uses in production here), so WSPR builds for both the native
 * server and the browser WASM runtime.
 *
 * The vendored `wsprd.c` keeps its `#include <fftw3.h>` byte-identical
 * for clean re-syncs; build.rs places `shim/` on the include path so
 * this resolves instead of any system FFTW. The only other vendor
 * delta is the two FFTW-wisdom file blocks (`#if 0`'d — meaningless
 * without FFTW and filesystem-free under WASM anyway).
 */
#ifndef FERRITE_WSPRD_FFTW3_SHIM_H
#define FERRITE_WSPRD_FFTW3_SHIM_H

#include <stdlib.h>
#include "kiss_fft.h"

#ifdef __cplusplus
extern "C" {
#endif

/* FFTW's fftwf_complex is `float[2]` ([0]=re, [1]=im) — binary
 * identical to kiss_fft_cpx {float r; float i;}, so the execute
 * boundary is a reinterpret-cast, no element copy. */
typedef float fftwf_complex[2];

#define FFTW_FORWARD  (-1)
#define FFTW_ESTIMATE (0u)

typedef struct ferrite_fftwf_plan_s {
    kiss_fft_cfg         cfg;
    const fftwf_complex *in;
    fftwf_complex       *out;
} *fftwf_plan;

static inline void *fftwf_malloc(size_t n) { return malloc(n); }
static inline void  fftwf_free(void *p)    { free(p); }

static inline fftwf_plan
fftwf_plan_dft_1d(int n, fftwf_complex *in, fftwf_complex *out,
                  int sign, unsigned flags) {
    (void)flags;
    fftwf_plan p = (fftwf_plan)malloc(sizeof(*p));
    if (!p) return NULL;
    /* FFTW_FORWARD(-1) → kiss inverse flag 0 (forward transform). */
    p->cfg = kiss_fft_alloc(n, sign == FFTW_FORWARD ? 0 : 1, NULL, NULL);
    p->in  = in;
    p->out = out;
    return p;
}

static inline void fftwf_execute(fftwf_plan p) {
    if (p && p->cfg)
        kiss_fft(p->cfg,
                 (const kiss_fft_cpx *)p->in,
                 (kiss_fft_cpx *)p->out);
}

static inline void fftwf_destroy_plan(fftwf_plan p) {
    if (p) { kiss_fft_free(p->cfg); free(p); }
}

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_WSPRD_FFTW3_SHIM_H */
