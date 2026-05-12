// Tiny glue layer between ft8_lib and the Rust wrapper.
//
// ft8_lib's `monitor_t` is a sizeable concrete struct (kiss_fft state,
// waterfall scratch, window table) that the upstream API expects the
// caller to allocate and pass by pointer. Mirroring its layout in
// Rust would couple to upstream's struct packing — so instead we
// expose three thin glue functions that hide the struct entirely.
// Rust deals only with opaque `*mut monitor_t`, and the rest of the
// FT8 API takes a `const ftx_waterfall_t*` we hand back from
// `ferrite_monitor_waterfall`.

#include <stdlib.h>
#include <common/monitor.h>
#include <ft8/decode.h>

monitor_t* ferrite_monitor_create(const monitor_config_t* cfg)
{
    if (!cfg) return NULL;
    monitor_t* m = (monitor_t*)malloc(sizeof(monitor_t));
    if (!m) return NULL;
    monitor_init(m, cfg);
    return m;
}

void ferrite_monitor_destroy(monitor_t* m)
{
    if (!m) return;
    monitor_free(m);
    free(m);
}

void ferrite_monitor_reset(monitor_t* m)
{
    if (m) monitor_reset(m);
}

void ferrite_monitor_process(monitor_t* m, const float* frame)
{
    if (m && frame) monitor_process(m, frame);
}

// Block size in samples — caller feeds exactly this many `float`s
// per call to `ferrite_monitor_process`. Exposed because the Rust
// side needs to chunk its input to match.
int ferrite_monitor_block_size(const monitor_t* m)
{
    return m ? m->block_size : 0;
}

// Number of FFT blocks (= waterfall rows) currently filled. Hits
// `max_blocks` when a full slot has been ingested; that's when the
// Rust side calls find_candidates + decode_candidate.
int ferrite_monitor_blocks_filled(const monitor_t* m)
{
    return m ? m->wf.num_blocks : 0;
}

int ferrite_monitor_blocks_max(const monitor_t* m)
{
    return m ? m->wf.max_blocks : 0;
}

// Hand back a const pointer to the embedded waterfall struct. The
// FT8 decode path (`ftx_find_candidates`, `ftx_decode_candidate`)
// takes this pointer directly. Caller must not retain past a
// `monitor_destroy`.
const ftx_waterfall_t* ferrite_monitor_waterfall(const monitor_t* m)
{
    return m ? &m->wf : NULL;
}
