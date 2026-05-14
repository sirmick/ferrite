/* Minimal signal.h stub for wasm32-unknown-unknown.
 *
 * wasi-libc's `<signal.h>` errors out unless `_WASI_EMULATED_SIGNAL` is
 * defined, and pulling that in means also linking
 * `lwasi-emulated-signal.a`. Ferrite never raises signals from inside
 * a block — the only reason `<signal.h>` reaches a vendor source at
 * all is via `sig_atomic_t` in `r_cfg`'s exit-flag fields, which are
 * only relevant to the upstream main loop we don't compile.
 *
 * Stub just enough so the typedef is visible. The fields stay zero
 * forever in our use.
 */

#ifndef FERRITE_LIBC_STUBS_SIGNAL_H
#define FERRITE_LIBC_STUBS_SIGNAL_H

#ifdef __cplusplus
extern "C" {
#endif

typedef int sig_atomic_t;

/* Forward-declared signal numbers some vendors mention in headers. We
 * never raise/handle, so values are placeholders. */
#define SIGINT  2
#define SIGTERM 15
#define SIGPIPE 13

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_SIGNAL_H */
