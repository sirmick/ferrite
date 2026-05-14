/* Minimal mongoose.h stub for Ferrite's rtl_433 lift.
 *
 * Upstream rtl_433 vendors the full Mongoose (cesanta.com) embedded
 * HTTP/networking library, which it uses for `-F http`, `-F mqtt`,
 * `-F influx`, and the GPSD-tag (`data_tag.c`) features. Ferrite ships
 * none of these — output runs through our own event ring under
 * `decoder::rtl_433` tracing; tag-output is dropped entirely (see
 * VENDOR.md, `data_tag.c` removed).
 *
 * `r_api.c` still includes `mongoose.h` at the top of the file and has
 * two functions (`get_mgr`, `add_http_output`) that mention `mg_mgr` /
 * `mg_mgr_init` / `mg_mgr_free`. The vendor-port-guide rule is "don't
 * fork upstream files", so instead of patching r_api.c we put this
 * shim header on the include path *before* the vendor copy and let it
 * win. The functions in r_api.c still compile (they parse + typecheck),
 * never get called from any active code path, and get dropped by
 * `--gc-sections` at link time.
 *
 * If a future upstream bump adds new mongoose API references inside
 * code paths Ferrite *does* reach, the failure mode is an undefined-
 * symbol link error pointing at the new mg_* symbol — at which point
 * the right fix is either declaring it here (and never calling it) or
 * dropping the new feature like we did with `data_tag.c`.
 */

#ifndef FERRITE_RTL433_MONGOOSE_H
#define FERRITE_RTL433_MONGOOSE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MG_VERSION "6.16-ferrite-stub"

/* Opaque enough that the linker won't complain, big enough that
 * `sizeof(struct mg_mgr)` in r_api.c's `calloc(1, sizeof(...))` returns
 * a non-zero value. We never read these bytes. */
struct mg_mgr {
    void *_ferrite_unused[8];
};

/* The two functions r_api.c calls from its HTTP path. The
 * implementations live nowhere — these are pure declarations; the
 * linker only complains if dead-code elimination fails to drop the
 * caller, which it doesn't for the rtl_433 use case (no HTTP output
 * ever registered). */
void mg_mgr_init(struct mg_mgr *mgr, void *user_data);
void mg_mgr_free(struct mg_mgr *mgr);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_RTL433_MONGOOSE_H */
