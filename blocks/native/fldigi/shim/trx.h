/* Shim replacement for fldigi's <trx.h>.
 * The vendored modems reference the trx globals (`trx_state`,
 * `active_modem`) but we drive rx_process() ourselves from the C ABI —
 * fldigi's own trx thread/loop is not compiled. `state_t` lives in the
 * (vendored, clean) globals.h. fldigi_shim.cxx defines the globals. */
#ifndef FERRITE_FLDIGI_SHIM_TRX_H
#define FERRITE_FLDIGI_SHIM_TRX_H

#include <cstddef>
#include "globals.h"   /* state_t, trx_mode — vendored, FLTK-free */
#include "modem.h"     /* vendored */

extern state_t  trx_state;
extern modem   *active_modem;
extern bool     bHistory;
extern bool     bHighSpeed;
extern bool     rx_only;

extern void trx_start_modem(modem *m, int f = 0);
extern void trx_start(void);
extern void trx_close(void);
extern void trx_transmit(void);
extern void trx_tune(void);
extern void trx_receive(void);
extern void trx_reset(void);
extern void trx_wait_state(void);

extern void trx_xmit_wfall_queue(int samplerate, const double *buf, size_t len);

#define TRX_WAIT(s_, code_) do { code_; } while (0)

#endif /* FERRITE_FLDIGI_SHIM_TRX_H */
