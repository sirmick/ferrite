// Minimal `wasi_snapshot_preview1` import shim.
//
// `ferrite-blocks` (and any future C-vendor block in `blocks/native/*`)
// links wasi-libc's `libc.a` for `malloc` / `free` / `cexpf` etc. on
// the wasm32-unknown-unknown target. wasi-libc's `libc.a` references
// a handful of WASI syscalls (`fd_write`/`fd_close`/`fd_seek`/
// `fd_fdstat_get`) for the stdio path that liquid-dsp doesn't actually
// exercise — but the linker can't prove that, so the WASM imports
// them anyway.
//
// `wasm32-unknown-unknown` doesn't provide a WASI runtime, so the
// browser would normally fail with `ERR_MODULE_NOT_FOUND` when the
// wasm-bindgen JS shim does `import * as _ from "wasi_snapshot_preview1"`.
// We satisfy the import with this stub: every function is a no-op
// returning `0` (≈ "ok"). The longer-term cleanup is to link a
// printf-less allocator (dlmalloc) into the libc archive so these
// imports go away entirely; until then, no-op stubs are correct
// because the code paths that would call them never run.
//
// Wired in via `vite.config.ts`'s `resolve.alias`, so the wasm-bindgen
// shim's bare `import "wasi_snapshot_preview1"` resolves here at build
// time.

const noop = (): number => 0;

export const fd_close = noop;
export const fd_fdstat_get = noop;
export const fd_seek = noop;
export const fd_write = noop;
// Misc others wasi-libc occasionally references — keep the surface
// generous so adding a new vendor doesn't surprise us.
export const fd_read = noop;
export const fd_prestat_get = noop;
export const fd_prestat_dir_name = noop;
export const environ_get = noop;
export const environ_sizes_get = noop;
export const proc_exit = (_code: number): void => {
  // Don't actually exit — liquid's `liquid_error_fl` calls into here
  // on hard failures we'd like to surface as exceptions. Silently
  // returning matches our no-op stance and keeps the WASM running.
};
export const random_get = noop;
export const clock_time_get = noop;
