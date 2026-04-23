/* Minimal stdio.h stub for wasm32-unknown-unknown C vendoring.
 *
 * wasi-libc's <stdio.h> drags in <wasi/api.h>, which is gated on the
 * `wasm32-wasi` ABI and refuses to compile under `wasm32-unknown-unknown`.
 * We only need stdio for liquid-dsp's diagnostic print paths
 * (fprintf(stderr, …) on errors, snprintf for label formatting). Those
 * paths never execute on a sane DSP block, but they have to compile and
 * link.
 *
 * This header gives the compiler enough to parse declarations and
 * expressions that mention stdio types. Real implementations are linked
 * from `libc-stubs/stubs.c` and either no-op or write into a per-block
 * event ring once `ferrite_port.h` redirects them.
 *
 * Native (non-wasm) builds use the system stdio.h via the normal include
 * path — this stub only sits in front of wasi-libc on the wasm32 path.
 */

#ifndef FERRITE_LIBC_STUBS_STDIO_H
#define FERRITE_LIBC_STUBS_STDIO_H

#include <stddef.h>  /* size_t — wasi-libc's stddef.h has no WASI gate */
#include <stdarg.h>  /* va_list — clang built-in, no libc dep */

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque FILE handle. Real wasi-libc would expose internals; we never
 * touch them, so an incomplete type is enough. */
typedef struct __ferrite_stub_FILE FILE;

/* Symbol stand-ins for stderr/stdout — never written to in DSP code,
 * but liquid mentions them in error printfs. The "_ferrite_stub_*"
 * prefix avoids clashing with anything wasi-libc would expose later. */
extern FILE *stderr;
extern FILE *stdout;
extern FILE *stdin;

/* Declarations of every stdio function liquid (or any reasonable
 * vendor) might reference. All resolve to no-ops at link time via
 * libc-stubs/stubs.c. Return types match POSIX so existing code parses
 * unchanged. */
int    printf(const char *fmt, ...);
int    fprintf(FILE *stream, const char *fmt, ...);
int    snprintf(char *buf, size_t cap, const char *fmt, ...);
int    sprintf(char *buf, const char *fmt, ...);
int    vfprintf(FILE *stream, const char *fmt, va_list ap);
int    vprintf(const char *fmt, va_list ap);
int    vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap);
int    fputs(const char *s, FILE *stream);
int    fputc(int c, FILE *stream);
int    putchar(int c);
int    puts(const char *s);

/* fopen/fclose/fread/fwrite — liquid's logger touches them. We provide
 * declarations so it compiles; the stub implementations refuse to open
 * anything and the logger paths short-circuit. */
FILE * fopen(const char *path, const char *mode);
int    fclose(FILE *stream);
size_t fread(void *buf, size_t size, size_t n, FILE *stream);
size_t fwrite(const void *buf, size_t size, size_t n, FILE *stream);
int    fflush(FILE *stream);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_STDIO_H */
