/* multimon-ng IO shim — replaces unixinput.c's globals + _verbprintf
 * so the decoders can run as a library instead of a CLI.
 *
 * upstream multimon-ng is GPL-2-or-later (Tom Sailer / Elias Oenal et
 * al). This shim is part of ferrite and is GPL-3-or-later under the
 * project's overall license; the two are compatible.
 *
 * THREADING — the buffer is `__thread`, so each thread that ticks a
 * multimon decoder gets its own. Within a thread, the runtime serialises
 * block process() calls and the wrapper drains the buffer immediately
 * after each demod() invocation, so two decoder instances never see
 * each other's bytes.
 */

#include "multimon_shim.h"

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* --- Globals demods reference (mirroring unixinput.c). --------------- */

/* All decoders unconditionally call _verbprintf at level 0. We default
 * to 1 so the lines reach our buffer; verbosity filtering is done
 * Rust-side after capture. */
int verbose_level = 1;
int integer_only = 0;
bool dont_flush = false;
bool is_startline = true; /* multimon uses this to decide whether to
                            prepend `label: ` and a timestamp. We don't
                            want either, so leaving startline true is
                            harmless because label is NULL and timestamp
                            is 0. */
int timestamp = 0;
int iso8601 = 0;
char *label = NULL;
int json_mode = 0;
int flex_disable_timestamp = 0;
bool fms_justhex = false;

/* unixinput.c's `quit()` exits the process. Library callers don't want
 * that — the runtime handles lifecycle. Stub to a no-op. */
void quit(void) {}

/* unixinput.c's `addJsonTimestamp` adds a "timestamp" field to a
 * cJSON object. Decoders only call it when json_mode != 0, but the
 * linker still wants the symbol. Stub to a no-op since we never enter
 * those branches. The forward decl avoids pulling in cJSON.h here. */
struct cJSON;
void addJsonTimestamp(struct cJSON *json_output) { (void)json_output; }

/* --- Per-thread capture buffer -------------------------------------- */

#define MULTIMON_BUFFER_CAP 65536u
static __thread char multimon_buffer[MULTIMON_BUFFER_CAP];
static __thread size_t multimon_buffer_len = 0;

/* `_verbprintf` is called from inside multimon's demods. We replicate
 * its API but redirect the formatted bytes into the thread-local buffer
 * instead of stdout. */
void _verbprintf(int verb_level, const char *fmt, ...) {
    if (verb_level > verbose_level) return;
    va_list args;
    va_start(args, fmt);
    if (multimon_buffer_len < MULTIMON_BUFFER_CAP - 1) {
        size_t remaining = MULTIMON_BUFFER_CAP - multimon_buffer_len;
        int n = vsnprintf(multimon_buffer + multimon_buffer_len,
                          remaining, fmt, args);
        if (n > 0) {
            multimon_buffer_len += (size_t)n < remaining
                                       ? (size_t)n
                                       : (remaining - 1);
        }
    }
    va_end(args);
}

size_t multimon_drain(char *dst, size_t cap) {
    size_t n = multimon_buffer_len < cap ? multimon_buffer_len : cap;
    if (n > 0 && dst != NULL) {
        memcpy(dst, multimon_buffer, n);
    }
    multimon_buffer_len = 0;
    return n;
}

void multimon_reset_buffer(void) {
    multimon_buffer_len = 0;
}

/* --- POCSAG family setters ------------------------------------------ */

/* Forward-declare the vendor globals we mutate. Defined in
 * vendor/pocsag.c; bindgen can't see them because they're not in
 * any header, so we wrap them in functions and expose those instead. */
extern int pocsag_show_partial_decodes;
extern int pocsag_polarity;

void multimon_pocsag_set_show_partial(int enabled) {
    pocsag_show_partial_decodes = enabled ? 1 : 0;
}

void multimon_pocsag_set_polarity(int mode) {
    pocsag_polarity = mode;
}
