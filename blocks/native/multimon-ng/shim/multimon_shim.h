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

#ifdef __cplusplus
}
#endif

#endif
