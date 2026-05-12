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

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_LIBC_STUBS_TIME_H */
