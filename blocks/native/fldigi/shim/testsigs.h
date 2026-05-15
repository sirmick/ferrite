/* Shim for <testsigs.h> — TX IMD test-signal widgets. psk.cxx reads
   btn_imd_on->value() / xmtimd->value() but only inside a
   `test_signal_window && ->visible()` guard (never true headless).
   Reuse the generic no-op valuator from fl_digi.h. */
#ifndef FERRITE_FLDIGI_SHIM_TESTSIGS_H
#define FERRITE_FLDIGI_SHIM_TESTSIGS_H
#include "fl_digi.h"
extern _shim_valuator *btn_imd_on;
extern _shim_valuator *xmtimd;
#endif
