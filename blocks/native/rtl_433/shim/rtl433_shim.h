/* Ferrite ↔ rtl_433 ABI surface.
 *
 * Five entry points wrap upstream's pulse-detect + decoder dispatch into
 * something a Rust block can drive. No callbacks across the FFI boundary:
 * decoded events sit in an internal JSON ring and the Rust side polls
 * via `rtl433_drain_event` after each `rtl433_push_iq`. Same idiom as
 * `multimon_drain` in the multimon-ng shim.
 *
 * Threading: one `rtl433_state_t` is owned by a single block instance
 * and only touched from the thread that built it. Multiple instances on
 * separate threads are independent (the upstream state structs are
 * per-cfg). Inside one thread, the runtime's serialised tick loop
 * provides the rest of the safety.
 */

#ifndef FERRITE_RTL433_SHIM_H
#define FERRITE_RTL433_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle. Backed by an upstream `r_cfg_t *` plus our event ring;
 * Rust only ever sees the pointer. */
typedef struct rtl433_state rtl433_state_t;

/* Build a decoder instance at the given input sample rate (250 kHz is
 * upstream's default and the right choice for almost every ISM band).
 *
 * `decoder_threshold` selects which subset of upstream's ~320 decoders
 * to register, by matching against each `r_device.disabled` field:
 *   0 = default-enabled only (~220 stable decoders) — upstream default
 *   1 = also experimental / niche / noisy decoders
 *   3 = also broken / hidden decoders
 * Threshold maps 1:1 to the `unsigned disabled` argument upstream's
 * `register_all_protocols` takes.
 *
 * Returns NULL on allocation failure. */
rtl433_state_t *rtl433_init(uint32_t sample_rate_hz, uint8_t decoder_threshold);

/* Tear down all upstream state + the event ring. */
void rtl433_free(rtl433_state_t *state);

/* Reset the pulse detector + FM demodulator state. Used between
 * unrelated capture segments; not normally needed during streaming. */
void rtl433_reset(rtl433_state_t *state);

/* Feed `n_complex` interleaved I/Q samples (f32 in ±1.0 convention,
 * the Ferrite IQ port standard). The shim scales to int16 CS16
 * internally, chunks against the upstream `MAXIMAL_BUF_LENGTH` ceiling,
 * runs envelope+FM demod + pulse detection, and dispatches matched
 * decoders. Decoded device frames land in the internal JSON ring as
 * one entry per `data_t` the decoder produces. */
void rtl433_push_iq(rtl433_state_t *state, const float *iq, size_t n_complex);

/* Pop one decoded event from the ring as a NUL-terminated JSON string.
 * Returns the number of bytes written (excluding NUL), 0 if the ring
 * is empty, or a negative value on caller-buffer-too-small (the event
 * is dropped — bump `cap` if this fires). Events longer than the
 * shim's per-slot cap (RTL433_EVENT_MAX_BYTES) are dropped silently
 * at push time. */
int rtl433_drain_event(rtl433_state_t *state, char *dst, size_t cap);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_RTL433_SHIM_H */
