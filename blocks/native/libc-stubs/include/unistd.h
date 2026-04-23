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

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_UNISTD_H */
