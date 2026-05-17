/* Drain interface exposed to Rust.
 *
 * multimon-ng decoders write their output through `_verbprintf` (which
 * we re-implement in `multimon_shim.c` to capture instead of printing).
 * The Rust side calls `multimon_drain` after each `demod()` invocation
 * to harvest whatever the decoder produced and turn it into a stream
 * of complete lines.
 */
#ifndef MULTIMON_SHIM_H
#define MULTIMON_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Copy up to `cap` bytes of buffered verbprintf output into `dst`.
 * Returns the number of bytes copied. Always resets the internal
 * buffer afterward, regardless of whether `dst` had space — partial
 * truncation is acceptable; we cap the buffer at 64 KB and decoders
 * never approach that within a single tick. */
size_t multimon_drain(char *dst, size_t cap);

/* Hard-reset the buffer without reading. Used on block reset. */
void multimon_reset_buffer(void);

/* POCSAG family setters — write the corresponding global declared
 * in vendor/pocsag.c. Centralised here so bindgen has a real header
 * declaration to wrap (it can't see pure-.c globals). */
void multimon_pocsag_set_show_partial(int enabled);
void multimon_pocsag_set_polarity(int mode);

/* Switch AX.25 display to TNC2 APRS form (`APRS: SRC>DEST,path:info`).
 * See multimon_shim.c for why this is safe for non-APRS decoders. */
void multimon_set_aprs_mode(int on);

#ifdef __cplusplus
}
#endif

#endif
