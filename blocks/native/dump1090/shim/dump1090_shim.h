/* Library IO replacement for the vendored dump1090.c.
 *
 * Antirez's dump1090 is built as a CLI binary: `main()` opens an RTL-SDR
 * (or stdin file), spins a reader thread, runs the demod inline, and
 * writes decoded frames to stdout / network sockets. None of that is
 * useful inside a tick-driven runtime, so this shim does three jobs:
 *
 *  1. **Type stubs** — provides empty typedefs and no-op macros for the
 *     pthread / RTL-SDR / anet types referenced in the global `Modes`
 *     struct, so the file compiles without those headers. The fields
 *     these types decorate are never read once the SDR + network code
 *     paths are excised, so the empty types are safe.
 *  2. **Output capture** — `dump1090_emit_text` is a printf-compatible
 *     buffered writer. dump1090.c `#define printf dump1090_emit_text`
 *     near the top so every text emission (displayModesMessage's many
 *     printf calls, raw-mode dumps, debug prints) lands in a per-thread
 *     ring instead of stdout. `dump1090_drain` empties that ring; the
 *     Rust wrapper calls it after each push.
 *  3. **Library entry points** — `dump1090_init`, `dump1090_push_iq`,
 *     `dump1090_drain` form the safe surface bindgen exposes.
 */

#ifndef FERRITE_DUMP1090_SHIM_H
#define FERRITE_DUMP1090_SHIM_H

#include <stddef.h>
#include <stdint.h>

/* --- Type stubs for excised headers ----------------------------------
 *
 * The pthread types still arrive transitively through sys/types.h →
 * pthreadtypes.h, so we don't redefine them — but every pthread *call*
 * inside the keep-zone (modesInit's mutex/cond init, the reader-thread
 * lock pairs we couldn't fully cull) is no-op'd via macros. dump1090
 * runs single-threaded inside the runtime tick loop; mutexes have
 * nothing to guard. `rtlsdr_dev_t` and the anet types belong to
 * libraries we don't link against, so we provide opaque stubs.
 *
 * Why a real pthread call won't fire: every `pthread_*` callsite that
 * remains in the keep-zone is inside `modesInit` (the mutex/cond
 * initialisers); the reader-thread side of those primitives is in the
 * `#if 0` block. So even though the types remain real, the runtime
 * cost is zero and there's no thread to schedule. */

typedef struct rtlsdr_dev rtlsdr_dev_t;

#define pthread_mutex_init(m, a)  ((void)(m), (void)(a), 0)
#define pthread_cond_init(c, a)   ((void)(c), (void)(a), 0)
#define pthread_mutex_lock(m)     ((void)(m), 0)
#define pthread_mutex_unlock(m)   ((void)(m), 0)
#define pthread_cond_wait(c, m)   ((void)(c), (void)(m), 0)
#define pthread_cond_signal(c)    ((void)(c), 0)
#define pthread_create(t, a, f, x) ((void)(t), (void)(a), (void)(f), (void)(x), 0)
#define pthread_join(t, r)        ((void)(t), (void)(r), 0)

#define ANET_ERR_LEN 256

/* --- Output capture --------------------------------------------------- */

#ifdef __cplusplus
extern "C" {
#endif

/* printf-compatible writer. dump1090.c `#define printf dump1090_emit_text`
 * routes every text emission through here into a per-thread ring. */
int dump1090_emit_text(const char *fmt, ...);

/* Drain the per-thread ring into `dst` (capped at `cap`). Returns bytes
 * written; resets the buffer. Lines are `\n`-terminated; the Rust side
 * splits and forwards them as `decoder::adsb` tracing events. Same shape
 * as `multimon_drain` so the ferrite block code can stay symmetric. */
size_t dump1090_drain(char *dst, size_t cap);

/* --- Library entry points -------------------------------------------- */

/* One-time global init: builds the maglut, error-syndrome table, and
 * resets the aircraft list. Must be called before any push. Idempotent
 * — second call is a no-op. */
void dump1090_init(void);

/* Push one block of u8 interleaved I/Q samples (RTL-SDR's native format,
 * 127 = zero) at 2 MS/s. Length is the byte count = 2 × sample count.
 * Internally builds the magnitude vector, runs `detectModeS`, and any
 * decoded frames land in the per-thread output buffer. */
void dump1090_push_iq_u8(const unsigned char *iq, size_t bytes);

/* Reset the per-thread ring and the global aircraft list. Used between
 * unrelated capture sessions in the analyzer. */
void dump1090_reset(void);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_DUMP1090_SHIM_H */
