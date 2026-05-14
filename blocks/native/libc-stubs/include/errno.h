/* Minimal errno.h stub for wasm32-unknown-unknown.
 *
 * wasi-libc has a working `<errno.h>` but `-nostdlibinc` plus the
 * stub-first include order means we end up here first when a vendor
 * file does `#include <errno.h>`. Provide the minimum needed for the
 * vendor paths to compile; the actual error-handling code rarely
 * reaches a hot path inside a block (it's error logging on file I/O
 * we don't perform).
 */

#ifndef FERRITE_LIBC_STUBS_ERRNO_H
#define FERRITE_LIBC_STUBS_ERRNO_H

#ifdef __cplusplus
extern "C" {
#endif

/* Thread-local errno per POSIX. Backed by a real symbol in wasi-libc;
 * declared `extern int` here so the compiler accepts `errno = N` and
 * `if (errno == X)` references. */
extern int *__errno_location(void);
#define errno (*__errno_location())

/* The errno values rtl_433's vendor code actually mentions. Numeric
 * codes match Linux for the common ones; identity-only otherwise. */
#define EPERM        1
#define ENOENT       2
#define EINTR        4
#define EIO          5
#define EBADF        9
#define EAGAIN      11
#define ENOMEM      12
#define EACCES      13
#define EEXIST      17
#define EINVAL      22
#define ENOTSUP    134
#define EOVERFLOW   75
#define EWOULDBLOCK EAGAIN

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_ERRNO_H */
