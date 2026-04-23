/* Link-time stubs for the stdio.h / time.h surface declared in
 * libc-stubs/include/. Built into a tiny static library that every
 * blocks/native/<vendor>/ links against on the wasm32 path.
 *
 * Most calls are no-ops returning 0/NULL — DSP-vendor code usually
 * reaches them only on error paths that don't actually fire. The few
 * that need to do something (snprintf-into-a-buffer when liquid formats
 * a label) get a tiny in-place implementation rather than a no-op.
 */

#include <stddef.h>
#include <stdarg.h>
#include "stdio.h"
#include "time.h"

/* Sentinel pointers for the global stream symbols. liquid writes
 * `fprintf(stderr, …)`; our fprintf no-ops, so what `stderr` resolves
 * to doesn't matter as long as the symbol exists. We use the address of
 * a small static object so taking `&stderr` (rare, but legal) doesn't
 * land on null. */
static char __ferrite_stub_stderr_obj;
static char __ferrite_stub_stdout_obj;
static char __ferrite_stub_stdin_obj;
FILE *stderr = (FILE *)&__ferrite_stub_stderr_obj;
FILE *stdout = (FILE *)&__ferrite_stub_stdout_obj;
FILE *stdin  = (FILE *)&__ferrite_stub_stdin_obj;

/* All the print-style functions discard their arguments. liquid uses
 * fprintf for one-shot library-version mismatches we don't hit. */
int printf(const char *fmt, ...) { (void)fmt; return 0; }
int fprintf(FILE *s, const char *fmt, ...) { (void)s; (void)fmt; return 0; }
int vfprintf(FILE *s, const char *fmt, va_list ap) { (void)s; (void)fmt; (void)ap; return 0; }
int vprintf(const char *fmt, va_list ap) { (void)fmt; (void)ap; return 0; }
int fputs(const char *s, FILE *st) { (void)s; (void)st; return 0; }
int fputc(int c, FILE *st) { (void)st; return c; }
int putchar(int c) { return c; }
int puts(const char *s) { (void)s; return 0; }
int fflush(FILE *s) { (void)s; return 0; }

/* snprintf needs to actually format because liquid uses the result —
 * notably for object names. Bare-minimum implementation: return 0 if
 * buf or cap is bad, else write a single null terminator and return 0.
 * That makes liquid's labels empty strings, which is harmless.
 *
 * If we hit a vendor that genuinely needs formatted output, we can
 * pull in a real `snprintf` from a small embedded printf library. For
 * now this keeps the symbol resolved without ~10k of printf code. */
int snprintf(char *buf, size_t cap, const char *fmt, ...) {
    (void)fmt;
    if (buf && cap > 0) buf[0] = '\0';
    return 0;
}
int sprintf(char *buf, const char *fmt, ...) {
    (void)fmt;
    if (buf) buf[0] = '\0';
    return 0;
}
int vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap) {
    (void)fmt; (void)ap;
    if (buf && cap > 0) buf[0] = '\0';
    return 0;
}

/* File I/O stubs — refuse to open anything. */
FILE * fopen(const char *path, const char *mode) { (void)path; (void)mode; return NULL; }
int    fclose(FILE *s) { (void)s; return 0; }
size_t fread(void *buf, size_t size, size_t n, FILE *s) { (void)buf; (void)size; (void)n; (void)s; return 0; }
size_t fwrite(const void *buf, size_t size, size_t n, FILE *s) { (void)buf; (void)size; (void)n; (void)s; return n; }

/* Deterministic time stub — returns 0. PRNG seeding from `time(NULL)`
 * thus seeds from 0, which is fine for DSP but bad for cryptography
 * (we don't ship cryptography). */
time_t time(time_t *tloc) {
    if (tloc) *tloc = 0;
    return 0;
}
