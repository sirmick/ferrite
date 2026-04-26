/* IO replacement layer for the vendored aisdecoder.
 *
 * `ais_init` calls `init_ais_decoder` with NULL host/port and TCP/UDP
 * disabled — the vendor patches make those flags no-ops, but pinning
 * them here means future upstream-syncs that re-enable network code
 * still come up dark by default.
 *
 * `ais_push_audio` forwards into `run_rtlais_decoder`, which buffers in
 * chunks of `MAX_BUFFER_LENGTH` bytes internally. Caller chunk size is
 * irrelevant; we trust upstream's chunking loop.
 *
 * `ais_drain` walks the same `aisdecoder_next_message` linked-list the
 * vendor code already exposed. We just stitch each `!AIVDM,...*hh` line
 * onto a `\n`-separated buffer so the Rust side gets the same envelope
 * every other decoder wrap uses.
 */

#include "ais_shim.h"

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Forward decls of the vendor entry points (declared in
 * `vendor/aisdecoder.h` — but that header is minimal and we don't want
 * to pull `<netdb.h>` etc. via it). */
extern int init_ais_decoder(char *host, char *port, int show_levels,
                            int debug_nmea, int buf_len,
                            int time_print_stats, int use_tcp_listener,
                            int tcp_keep_ais_time, int add_sample_num);
extern void run_rtlais_decoder(short *buff, int len);
extern const char *aisdecoder_next_message(void);
extern int free_ais_decoder(void);

static bool g_initialised = false;

void ais_init(void) {
    if (g_initialised) return;
    /* Upstream's init: 4096-pair scratch is plenty for any tick chunk
     * we're likely to push (10 ms at 48 kHz = 480 pairs). show_levels=0,
     * debug_nmea=0, time_print_stats=0, use_tcp_listener=0,
     * tcp_keep_ais_time=0, add_sample_num=0. */
    (void)init_ais_decoder(NULL, NULL, 0, 0, 4096, 0, 0, 0, 0);
    g_initialised = true;
}

void ais_push_audio(const int16_t *interleaved_stereo, size_t n_pairs) {
    if (!g_initialised || interleaved_stereo == NULL || n_pairs == 0) return;
    /* `run_rtlais_decoder` takes a non-const pointer; the underlying
     * call path doesn't mutate the buffer (it `memcpy`s into a static
     * scratch). The cast is safe. */
    run_rtlais_decoder((short *)interleaved_stereo, (int)n_pairs);
}

size_t ais_drain(char *dst, size_t cap) {
    if (dst == NULL || cap == 0) return 0;
    size_t written = 0;
    const char *msg;
    while ((msg = aisdecoder_next_message()) != NULL) {
        size_t len = strlen(msg);
        /* Strip any trailing CR/LF — upstream emits "...,*hh\r\n";
         * we'll add our own '\n' separator to match the Rust drain
         * pattern (split on '\n', filter empty). */
        while (len > 0 && (msg[len - 1] == '\r' || msg[len - 1] == '\n')) {
            len--;
        }
        if (len == 0) continue;
        if (written + len + 1 > cap) break; /* truncate at cap */
        memcpy(dst + written, msg, len);
        written += len;
        dst[written++] = '\n';
    }
    return written;
}

void ais_reset(void) {
    if (!g_initialised) return;
    /* Pull every queued message and discard. `aisdecoder_next_message`
     * frees the previous message on the next call, so loop until NULL
     * and one extra call frees the last one. */
    while (aisdecoder_next_message() != NULL) { /* discard */ }
    (void)aisdecoder_next_message(); /* free trailing last_message */
}
