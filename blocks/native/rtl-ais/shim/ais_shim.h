/* Library IO replacement for the vendored aisdecoder.
 *
 * Three jobs:
 *  1. **Output capture** — `aisdecoder_next_message` already exists in
 *     the vendor source as a queue drain (the upstream rtl-ais binary
 *     uses it to pull NMEA sentences for its UDP/TCP bridge). We
 *     re-export that as a single `ais_drain` call that joins messages
 *     with `\n` so the Rust side gets the same "split on `\n`" envelope
 *     used by every other decoder wrap.
 *  2. **Library entry points** — `ais_init`, `ais_push_audio`,
 *     `ais_drain`, `ais_reset`. Same shape as `dump1090_*` and the
 *     multimon wrappers.
 *  3. **Suppress the network init** — `init_ais_decoder` originally took
 *     UDP host/port params; we hardcode NULL/NULL and rely on the
 *     vendor patches to skip the socket setup.
 *
 * AIS audio contract: interleaved s16 stereo at 48 kHz. Left = ch A
 * (161.975 MHz after FM-demod + resample), right = ch B (162.025 MHz).
 * The aisdecoder library runs an independent GMSK clock-recovery PLL
 * + AIVDM frame builder on each leg.
 */

#ifndef FERRITE_AIS_SHIM_H
#define FERRITE_AIS_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* One-time global init. Idempotent — second call is a no-op. */
void ais_init(void);

/* Push one block of interleaved s16 stereo audio at 48 kHz.
 * `n_pairs` is the number of left/right pairs (so the underlying
 * buffer holds `2 * n_pairs` shorts). The wrapper Rust code converts
 * f32 → s16 and interleaves before calling. */
void ais_push_audio(const int16_t *interleaved_stereo, size_t n_pairs);

/* Drain any complete AIVDM sentences emitted since the last call into
 * `dst` (capped at `cap`). Multiple sentences are joined with `\n`.
 * Returns bytes written; the queue is fully drained either way. Same
 * shape as `multimon_drain` / `dump1090_drain`. */
size_t ais_drain(char *dst, size_t cap);

/* Reset the message queue. Used between unrelated capture sessions
 * (the offline analyzer A/B-ing different fixtures). */
void ais_reset(void);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_AIS_SHIM_H */
