/* IO replacement layer for the vendored dump1090.
 *
 * Three jobs (see dump1090_shim.h preamble for context):
 *
 *  1. **printf capture** — `dump1090_emit_text` writes formatted bytes
 *     into a per-thread ring instead of stdout. dump1090.c's `printf`
 *     macro override routes every text emission here. Same shape as
 *     multimon-ng's `_verbprintf` shim, including the per-thread
 *     storage (one decoder = one tick-loop thread, no contention).
 *
 *  2. **Buffered IQ feed** — `dump1090_push_iq_u8` accumulates the
 *     caller's u8 IQ chunks into the dump1090 reader-buffer slot,
 *     mirroring the slide-and-fill pattern from `rtlsdrCallback` in
 *     the original code (slack of `(MODES_FULL_LEN-1)*4` bytes copied
 *     from the end of the previous batch to the start of the next so
 *     a frame straddling the boundary is still found).
 *
 *  3. **Lazy init** — `dump1090_init` calls upstream's `modesInitConfig`
 *     + `modesInit`, both of which assume single-threaded fresh-process
 *     state. Idempotent guard keeps the second call a no-op even when
 *     multiple Rust-side decoders are constructed (we only need one
 *     since the Modes struct is global).
 */

#include "dump1090_shim.h"

#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* --- 1. Per-thread output capture ------------------------------------ */

#define DUMP1090_BUFFER_CAP 65536u
static __thread char dump1090_buffer[DUMP1090_BUFFER_CAP];
static __thread size_t dump1090_buffer_len = 0;

int dump1090_emit_text(const char *fmt, ...) {
    va_list args;
    int written = 0;
    va_start(args, fmt);
    if (dump1090_buffer_len < DUMP1090_BUFFER_CAP - 1) {
        size_t remaining = DUMP1090_BUFFER_CAP - dump1090_buffer_len;
        int n = vsnprintf(dump1090_buffer + dump1090_buffer_len,
                          remaining, fmt, args);
        if (n > 0) {
            size_t added = (size_t)n < remaining ? (size_t)n : (remaining - 1);
            dump1090_buffer_len += added;
            written = (int)added;
        }
    }
    va_end(args);
    return written;
}

size_t dump1090_drain(char *dst, size_t cap) {
    size_t n = dump1090_buffer_len < cap ? dump1090_buffer_len : cap;
    if (n > 0 && dst != NULL) {
        memcpy(dst, dump1090_buffer, n);
    }
    dump1090_buffer_len = 0;
    return n;
}

/* --- 2. Library entry points ----------------------------------------- *
 *
 * These reach into the Modes struct + symbols defined in the vendored
 * dump1090.c, declared `extern` here so we don't repeat the includes.
 * The struct layout is private to dump1090.c — we only touch the few
 * fields we have to: `data` (the IQ scratch), `data_len` (its capacity),
 * `magnitude` (the converted vector), `aircrafts` (linked list to free
 * on reset). Forward declarations match the originals exactly. */

extern void modesInitConfig(void);
extern void modesInit(void);
extern void computeMagnitudeVector(void);
extern void detectModeS(uint16_t *m, uint32_t mlen);

/* The Modes struct is a translation-unit-local anonymous-typed global
 * in dump1090.c. We can't take its full type here — instead we declare
 * matching extern accessor variables for the pointers + counts the
 * shim needs. No: that won't work, the symbol is `Modes` (anonymous
 * type), but C lets us extern-declare individual offsetted members
 * only with the full struct type. So the shim has to live in the same
 * translation unit, OR dump1090.c has to expose helper functions.
 *
 * Cleanest path: add tiny accessor helpers to dump1090.c via an
 * `#ifdef FERRITE_LIB` block. We didn't do that — instead we'll define
 * a separate header that dump1090.c also includes, but skipping that
 * complication: we just use `extern char Modes[…]` byte-blob pattern.
 *
 * Actually simplest of all: the shim helpers are tiny C functions that
 * we add to the BOTTOM of dump1090.c via an `#include` from there.
 * Defined here, called via the `dump1090_*` symbols below. That keeps
 * dump1090.c clean and gives the shim direct access to Modes. */

/* IMPORTANT: this file is compiled separately from dump1090.c.
 * Therefore the helpers below cannot directly reference Modes.* fields
 * — they must call into helpers we add to dump1090.c. Since we can't
 * easily do that without modifying dump1090.c more, we instead expose
 * Modes via accessor functions defined in `dump1090_modes_access.c`
 * which `#include`s dump1090.c at the bottom (extending its file). */

/* Forward decls of the accessors implemented in
 * `dump1090_modes_access.c`. */
unsigned char *dump1090_modes_data_ptr(void);
size_t dump1090_modes_data_len(void);
void dump1090_modes_run_one_batch(size_t fresh_bytes);
void dump1090_modes_reset_aircraft_list(void);

static bool g_initialised = false;

void dump1090_init(void) {
    if (g_initialised) return;
    modesInitConfig();
    modesInit();
    g_initialised = true;
}

/* Pending fresh bytes accumulated into the data buffer slot, AFTER the
 * slack region the original code preserves at the front. The dump1090
 * buffer is sized `MODES_DATA_LEN + (MODES_FULL_LEN-1)*4`; we fill
 * starting from offset `(MODES_FULL_LEN-1)*4` and run a batch when we
 * have `MODES_DATA_LEN` fresh bytes. See `rtlsdrCallback` in the
 * original code for the slide-and-fill model we mirror. */
#define MODES_FULL_LEN_BYTES   ((8 + 112) * 4)  /* matches MODES_FULL_LEN*4 in dump1090.c */
#define MODES_DATA_BYTES       (16 * 16384)     /* matches MODES_DATA_LEN */
#define MODES_SLACK_BYTES      ((MODES_FULL_LEN_BYTES) - 4)

static __thread size_t pending_bytes = 0;

void dump1090_push_iq_u8(const unsigned char *iq, size_t bytes) {
    if (!g_initialised || iq == NULL || bytes == 0) return;
    unsigned char *base = dump1090_modes_data_ptr();
    if (base == NULL) return;
    while (bytes > 0) {
        size_t avail = (size_t)MODES_DATA_BYTES - pending_bytes;
        size_t take = bytes < avail ? bytes : avail;
        memcpy(base + MODES_SLACK_BYTES + pending_bytes, iq, take);
        pending_bytes += take;
        iq += take;
        bytes -= take;
        if (pending_bytes == (size_t)MODES_DATA_BYTES) {
            dump1090_modes_run_one_batch(MODES_DATA_BYTES);
            pending_bytes = 0;
        }
    }
}

void dump1090_reset(void) {
    pending_bytes = 0;
    dump1090_buffer_len = 0;
    if (g_initialised) {
        dump1090_modes_reset_aircraft_list();
    }
}
