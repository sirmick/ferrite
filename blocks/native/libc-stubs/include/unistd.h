/* Minimal unistd.h stub — wasi-libc's pulls in <wasi/api.h> via the
 * fcntl/seek headers, which gates on the wasm32-wasi ABI. liquid-dsp
 * (and most DSP-only vendors) only reach for <unistd.h> as a side
 * effect of including stdio.h-related headers; nothing in the DSP
 * paths actually calls open/read/close.
 *
 * If a future vendor genuinely needs syscalls (file I/O, fork, exec)
 * that's a sign it doesn't belong on the wasm32-unknown-unknown side
 * — push it server-only or vendor a sandboxed subset.
 *
 * See `blocks/native/README.md` and `libc-stubs/include/stdio.h`
 * for the wider rationale on why the substrate interposes ahead of
 * wasi-libc on the wasm32 path.
 */

#ifndef FERRITE_LIBC_STUBS_UNISTD_H
#define FERRITE_LIBC_STUBS_UNISTD_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Just the typedefs / declarations a vendor might mention in headers
 * we transitively include. We deliberately don't declare open/read/
 * close/fork — link errors there are a feature. */
typedef long ssize_t;
typedef int  pid_t;

/* access() — rtl_433's samp_grab.c checks for existing capture files
 * before opening. The capture path is never invoked in Ferrite, but the
 * file has to compile. F_OK is the only mode the vendor reaches for;
 * the others are declared for completeness against future vendors.
 * Resolves to wasi-libc's `access` at link time (returns -1 on most
 * paths under wasm32-wasi anyway, which is fine). */
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
int access(const char *path, int mode);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_UNISTD_H */
