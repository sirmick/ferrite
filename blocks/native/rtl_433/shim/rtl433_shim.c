/* Ferrite ↔ rtl_433 shim — implementation.
 *
 * Mirrors the subset of upstream `rtl_433.c`'s `sdr_callback` we need:
 * envelope+FM demod, pulse detection, OOK/FSK decoder dispatch. The
 * upstream CLI's networking, output-sink fan-out, file dumping, and
 * sample-grab paths are bypassed — decoded events go straight to a
 * per-instance JSON ring the Rust side drains.
 *
 * The five r_devs.output_fn callbacks all point at `shim_output_fn`
 * here, which serialises the decoder's `data_t` to JSON via upstream's
 * `data_print_jsons` and pushes onto the ring.
 */

#include "rtl433_shim.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include "rtl_433.h"
#include "r_api.h"
#include "r_private.h"
#include "r_device.h"
#include "data.h"
#include "data_tag.h"
#include "list.h"
#include "baseband.h"
#include "pulse_detect.h"

/* data_tag_apply stub. Upstream's `data_acquired_handler` (in r_api.c)
 * walks `cfg->data_tags` and calls `data_tag_apply` on each — that's the
 * GPSD-tag enrichment we dropped along with `data_tag.c`. The function
 * is never actually called at runtime (our `cfg->data_tags` list stays
 * empty, and our shim overrides `output_fn` so data_acquired_handler
 * doesn't fire on a real decode either), but `data_acquired_handler`'s
 * address gets taken inside `register_protocol`, so the linker can't
 * drop it via --gc-sections, and the unresolved symbol fails the link.
 *
 * A no-op stub satisfies the linker without changing behaviour. Returns
 * `data` unchanged. */
data_t *data_tag_apply(data_tag_t *tag, data_t *data, char const *filename)
{
    (void)tag;
    (void)filename;
    return data;
}

/* Largest single JSON event we keep. Upstream events are typically
 * 100–400 bytes; 4 KB is generous and bounds the ring at sensible
 * memory cost. */
#define RTL433_EVENT_MAX_BYTES 4096

/* Ring of N concurrent un-drained events before we start dropping the
 * oldest. 64 entries × 4 KB = 256 KB per instance — small. */
#define RTL433_RING_SLOTS 64

typedef struct {
    int  len;                              /* bytes used (excl. NUL); 0 = empty */
    char buf[RTL433_EVENT_MAX_BYTES];      /* NUL-terminated when len > 0 */
} rtl433_event_t;

struct rtl433_state {
    r_cfg_t *cfg;                          /* upstream r_create_cfg() handle */
    rtl433_event_t ring[RTL433_RING_SLOTS];
    int  ring_head;                        /* next slot to write */
    int  ring_tail;                        /* next slot to read */
    int  ring_count;                       /* slots currently filled */
    int  dropped_full;                     /* events dropped because ring was full */
    int  dropped_oversize;                 /* events dropped because JSON > MAX */

    /* IQ scratch — interleaved int16 CS16 chunks ≤ MAXIMAL_BUF_LENGTH. */
    int16_t *iq_i16;
    size_t   iq_i16_cap;                   /* in complex samples */
};

/* ------------------------------------------------------------------ */
/* Output hook                                                         */
/* ------------------------------------------------------------------ */

/* The output_fn upstream's registered protocols call when a decoder
 * produces a `data_t`. We own the `data` afterwards and must free it. */
static void shim_output_fn(r_device *decoder, data_t *data)
{
    rtl433_state_t *state = (rtl433_state_t *)decoder->output_ctx;
    if (!state) {
        data_free(data);
        return;
    }

    /* Serialise into a stack buffer first; if it fits, copy into the
     * next ring slot. Upstream's `data_print_jsons` returns the number
     * of bytes that would have been written if the buffer were big
     * enough — clamp to detect truncation. */
    char scratch[RTL433_EVENT_MAX_BYTES];
    size_t n = data_print_jsons(data, scratch, sizeof(scratch));
    if (n == 0 || n >= sizeof(scratch)) {
        state->dropped_oversize++;
        data_free(data);
        return;
    }

    if (state->ring_count >= RTL433_RING_SLOTS) {
        /* Ring full — drop oldest and advance tail. Better than
         * blocking; the upstream pulse-detect tick can't yield. */
        state->ring_tail = (state->ring_tail + 1) % RTL433_RING_SLOTS;
        state->ring_count--;
        state->dropped_full++;
    }

    rtl433_event_t *slot = &state->ring[state->ring_head];
    memcpy(slot->buf, scratch, n);
    slot->buf[n] = '\0';
    slot->len    = (int)n;

    state->ring_head = (state->ring_head + 1) % RTL433_RING_SLOTS;
    state->ring_count++;

    data_free(data);
}

/* ------------------------------------------------------------------ */
/* Public ABI                                                          */
/* ------------------------------------------------------------------ */

rtl433_state_t *rtl433_init(uint32_t sample_rate_hz, uint8_t decoder_threshold)
{
    rtl433_state_t *state = calloc(1, sizeof(*state));
    if (!state) {
        return NULL;
    }

    /* Build the upstream config + dm_state + pulse_detect + r_devs[]
     * list. ~44 MB allocation inside `r_create_cfg` (dm_state has
     * MAXIMAL_BUF_LENGTH-sized buffers inlined) — large but matches
     * upstream's own footprint when running. */
    state->cfg = r_create_cfg();
    if (!state->cfg) {
        free(state);
        return NULL;
    }

    state->cfg->samp_rate = sample_rate_hz;
    /* Drive the mag-est detector branch — matches upstream's default
     * for CS16 input. Without this the pulse detector treats the
     * envelope as amplitude rather than magnitude and the dB scale
     * shifts by 6 dB. */
    state->cfg->demod->use_mag_est = 1;
    /* sample_size = 4 indicates CS16 (4 bytes per complex sample, two
     * int16s). Upstream's `data_acquired_handler` looks at this for the
     * dB-scale heuristic in `calc_rssi_snr`. */
    state->cfg->demod->sample_size = 4;
    /* Use the original FSK pulse-detector heuristic. AUTO (the
     * upstream default) over-triggers FSK classification on short OOK
     * bursts (Acurite weather sensors, etc.) — the minmax detector's
     * FM-output threshold runs hot when the carrier sits even slightly
     * off baseband DC, and a single false-positive on the first pulse
     * of a burst commits the whole package to `run_fsk_demods`
     * (silently bypassing every OOK decoder). The OLD heuristic
     * requires sustained frequency departure across multiple pulses
     * and is markedly less aggressive on noisy or carrier-offset OOK. */
    state->cfg->fsk_pulse_detect_mode = FSK_PULSE_DETECT_OLD;
    pulse_detect_set_levels(state->cfg->demod->pulse_detect,
                            state->cfg->demod->use_mag_est,
                            state->cfg->demod->level_limit,
                            state->cfg->demod->min_level,
                            state->cfg->demod->min_snr,
                            state->cfg->demod->detect_verbosity);

    /* Register every decoder at or below the requested disabled-
     * threshold. After this call, `cfg->demod->r_devs` is populated;
     * each `r_device` carries upstream's `data_acquired_handler` as
     * `output_fn`. Walk the list and rewrite each one to point at our
     * ring writer instead. */
    register_all_protocols(state->cfg, decoder_threshold);

    for (void **iter = state->cfg->demod->r_devs.elems; iter && *iter; ++iter) {
        r_device *r_dev = *iter;
        r_dev->output_fn  = shim_output_fn;
        r_dev->output_ctx = state;
    }

    /* Pre-allocate the int16 IQ scratch. Sized for one tick's worth of
     * pushed samples; grown lazily in push_iq if needed. 64 K complex
     * samples is comfortable for any single block tick. */
    state->iq_i16_cap = 65536;
    state->iq_i16     = malloc(state->iq_i16_cap * 2 * sizeof(int16_t));
    if (!state->iq_i16) {
        rtl433_free(state);
        return NULL;
    }

    return state;
}

void rtl433_free(rtl433_state_t *state)
{
    if (!state) return;

    if (state->iq_i16) {
        free(state->iq_i16);
    }

    /* Upstream offers no symmetric `r_free_cfg`; the CLI exits the
     * process and lets the OS reclaim. Inside Ferrite we live longer,
     * so leak-only-on-exit isn't acceptable. Best-effort free of the
     * fields we know we allocated; the dm_state, devices array, and
     * cfg itself are direct calloc()s. The r_devs list contents are
     * `register_protocol`-malloc'd copies of `cfg->devices[i]`. */
    if (state->cfg) {
        r_cfg_t *cfg = state->cfg;

        /* `free_protocol` already calls `free(r_dev)` on each entry —
         * the upstream pattern is to pass it as `list_free_elems`'s
         * per-elem free callback rather than iterate manually. */
        list_free_elems(&cfg->demod->r_devs, (list_elem_free_fn)free_protocol);

        if (cfg->demod->pulse_detect) {
            pulse_detect_free(cfg->demod->pulse_detect);
        }
        free(cfg->demod);
        free(cfg->devices);
        free(cfg);
    }

    free(state);
}

void rtl433_reset(rtl433_state_t *state)
{
    if (!state || !state->cfg || !state->cfg->demod) return;

    struct dm_state *demod = state->cfg->demod;
    baseband_low_pass_filter_reset(&demod->lowpass_filter_state);
    baseband_demod_FM_reset(&demod->demod_FM_state);
    pulse_detect_reset(demod->pulse_detect);

    demod->min_level_auto = 0.0f;
    demod->noise_level    = 0.0f;

    /* Drop any pending events; a reset is a hard re-start. */
    state->ring_head  = 0;
    state->ring_tail  = 0;
    state->ring_count = 0;
}

void rtl433_push_iq(rtl433_state_t *state, const float *iq, size_t n_complex)
{
    if (!state || !state->cfg || !state->cfg->demod) return;
    if (n_complex == 0) return;

    /* Grow IQ scratch if the caller's chunk is larger than what we
     * currently hold. Realistic per-tick chunks are 4 K–32 K complex
     * samples; the lazy-grow path keeps memory tight for small ticks. */
    if (n_complex > state->iq_i16_cap) {
        free(state->iq_i16);
        state->iq_i16_cap = n_complex;
        state->iq_i16 = malloc(state->iq_i16_cap * 2 * sizeof(int16_t));
        if (!state->iq_i16) {
            state->iq_i16_cap = 0;
            return;
        }
    }

    /* Convert ±1.0 f32 IQ to ±32767 int16 CS16, interleaved I/Q. */
    {
        const float *src = iq;
        int16_t *dst = state->iq_i16;
        for (size_t i = 0; i < n_complex * 2; ++i) {
            float v = src[i] * 32767.0f;
            if (v > 32767.0f)  v = 32767.0f;
            if (v < -32768.0f) v = -32768.0f;
            dst[i] = (int16_t)v;
        }
    }

    struct dm_state *demod = state->cfg->demod;
    uint32_t samp_rate = state->cfg->samp_rate;
    /* Match rtl_433.c:531 — `cfg->demod->low_pass != 0.0f ? cfg->demod->low_pass
     * : fpdm ? 0.2f : 0.1f`. fpdm=AUTO so default is 0.2 unless overridden. */
    float low_pass = demod->low_pass != 0.0f
                       ? demod->low_pass
                       : (state->cfg->fsk_pulse_detect_mode ? 0.2f : 0.1f);

    /* Chunk the input into MAXIMAL_BUF_LENGTH-sized blocks. dm_state's
     * inlined `am_buf` and `buf.fm` cap at this size; pushing more in
     * one shot would overflow them. */
    size_t consumed = 0;
    while (consumed < n_complex) {
        size_t chunk = n_complex - consumed;
        if (chunk > MAXIMAL_BUF_LENGTH) chunk = MAXIMAL_BUF_LENGTH;

        int16_t const *iq_chunk = state->iq_i16 + (consumed * 2);
        uint32_t n_samples = (uint32_t)chunk;

        /* 1. Envelope (magnitude estimate) → demod->buf.temp (uint16_t).
         *    `temp` shares storage with `buf.fm`; we use temp for the
         *    OOK path, then re-purpose the union for FM after. */
        magnitude_est_cs16(iq_chunk, demod->buf.temp, n_samples);

        /* 2. Low-pass filter the envelope → demod->am_buf. */
        baseband_low_pass_filter(&demod->lowpass_filter_state,
                                 demod->buf.temp, demod->am_buf, n_samples);

        /* 3. FM demodulation → demod->buf.fm (same union storage as
         *    temp, now safe to overwrite — temp's content already
         *    fed the LPF in step 2). */
        baseband_demod_FM_cs16(&demod->demod_FM_state, iq_chunk, demod->buf.fm,
                               n_samples, samp_rate, low_pass);

        /* 4. Pulse detection + decoder dispatch. Upstream's
         *    `pulse_detect_package` is iteratively-resumable — it may
         *    yield multiple packages from one block of samples. Run
         *    until it returns 0 (no more packages this tick). */
        int package_type;
        do {
            package_type = pulse_detect_package(
                    demod->pulse_detect,
                    demod->am_buf, demod->buf.fm,
                    n_samples, samp_rate, state->cfg->input_pos,
                    &demod->pulse_data, &demod->fsk_pulse_data,
                    state->cfg->fsk_pulse_detect_mode);

            if (package_type == PULSE_DATA_OOK) {
                run_ook_demods(&demod->r_devs, &demod->pulse_data);
            } else if (package_type == PULSE_DATA_FSK) {
                run_fsk_demods(&demod->r_devs, &demod->fsk_pulse_data);
            }
        } while (package_type);

        state->cfg->input_pos += n_samples;
        consumed += chunk;
    }
}

int rtl433_drain_event(rtl433_state_t *state, char *dst, size_t cap)
{
    if (!state || !dst || cap == 0) return -1;
    if (state->ring_count == 0) return 0;

    rtl433_event_t *slot = &state->ring[state->ring_tail];
    int n = slot->len;
    if ((size_t)n + 1 > cap) {
        /* Caller-buffer too small. Drop the event so we don't deadlock
         * the ring on a permanently-stuck oversized record. */
        state->ring_tail = (state->ring_tail + 1) % RTL433_RING_SLOTS;
        state->ring_count--;
        return -2;
    }

    memcpy(dst, slot->buf, n);
    dst[n] = '\0';
    slot->len = 0;

    state->ring_tail = (state->ring_tail + 1) % RTL433_RING_SLOTS;
    state->ring_count--;

    return n;
}
