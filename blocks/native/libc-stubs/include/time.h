/* Minimal time.h stub — wasi-libc's pulls in <wasi/api.h> which
 * doesn't compile under wasm32-unknown-unknown. Liquid only uses time()
 * to seed PRNGs; that path is intercepted by ferrite_port.h to a
 * deterministic seed instead. See stdio.h for the wider rationale. */

#ifndef FERRITE_LIBC_STUBS_TIME_H
#define FERRITE_LIBC_STUBS_TIME_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Match wasi-libc's `__typedef_time_t.h` so a vendor that pulls our
 * time.h and a separate wasi header for some other reason doesn't trip
 * a typedef-redefinition error. */
typedef long long time_t;

/* Returns 0 stub-side; if you actually need wall time, do it from Rust. */
time_t time(time_t *tloc);

/* Just enough struct to let `localtime` etc. typecheck if any vendor
 * mentions them. We don't implement localtime — link errors there are
 * a feature, telling us which vendor needs further stubbing. */
struct tm {
    int tm_sec, tm_min, tm_hour;
    int tm_mday, tm_mon, tm_year;
    int tm_wday, tm_yday, tm_isdst;
};

/* multimon-ng's FLEX decoder calls `gmtime` to format timestamps in
 * decoded message metadata. The path runs only when the FLEX decoder
 * has a frame to log — in which case it's executed inside the Rust
 * shim's drain phase, so wasi-libc's real implementation is fine.
 * Declared so demod_flex.c compiles; resolves via libc.a at link. */
struct tm *gmtime(const time_t *timer);
struct tm *localtime(const time_t *timer);

/* mktime — rtl_433's geo_minim device decoder parses calendar fields
 * into a unix timestamp. Same story as gmtime above: declared here so
 * the device file compiles; the call runs inside the decode path which
 * fires on a real frame, and wasi-libc's `mktime` is fine. */
time_t mktime(struct tm *timeptr);

/* strftime — rtl_433's output paths and several device decoders format
 * timestamps. We never reach the upstream output writers, but the
 * device-side calls need to compile. */
size_t strftime(char *s, size_t maxsize, const char *format, const struct tm *timeptr);

/* localtime_r / gmtime_r — thread-safe variants used by rtl_433's
 * r_util.c for log timestamps. Declared here; same link-time resolution
 * via wasi-libc as the non-_r variants. */
struct tm *localtime_r(const time_t *timer, struct tm *result);
struct tm *gmtime_r(const time_t *timer, struct tm *result);

/* timegm — inverse of gmtime, used by r_util.c when re-encoding parsed
 * timestamps. POSIX extension, present in wasi-libc. */
time_t timegm(struct tm *tm);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_TIME_H */
