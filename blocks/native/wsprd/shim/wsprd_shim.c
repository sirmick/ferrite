/* wasm32-only libc shim for the vendored wsprd decoder.
 *
 * wsprd opens optional hashtable/wisdom files; every call site is
 * `if ((f = fopen(...)))`-guarded, so a NULL return cleanly skips them.
 *
 * Why this file exists: on `wasm32-unknown-unknown` there is no WASI
 * host. Statically linking wasi-libc's real `fopen` drags in its
 * `__wasilibc_find_relpath` / `__wasilibc_populate_preopens` object,
 * whose preopen-discovery loop (`__wasi_fd_prestat_get(fd++)` until
 * `BADF`) never terminates without a WASI runtime — it spins at module
 * init and hard-freezes the browser tab. Overriding `fopen` here means
 * the linker never pulls that object.
 *
 * Only `fopen` is overridden. `snprintf` (×33, callsign/grid
 * formatting), `printf`, `fclose`, `fgets`, `fprintf`, `sscanf` are
 * left to real wasi-libc so decode output stays correct — and with
 * `fopen` returning NULL the file-I/O paths that would call them never
 * execute anyway.
 *
 * build.rs compiles this ONLY for the wasm32 target; native wsprd uses
 * the real libc `fopen`.
 */

#include <stdio.h>

FILE *fopen(const char *path, const char *mode) {
    (void)path;
    (void)mode;
    return NULL;
}
