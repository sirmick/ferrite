/* Shim for <sys/timerfd.h> — Linux-only, absent from Emscripten's
 * sysroot. fsk.cxx #includes it but uses no timerfd_* symbols (the
 * io_timer path is commented out upstream), so an empty stub is
 * correct on every target. */
#ifndef FERRITE_FLDIGI_SHIM_SYS_TIMERFD_H
#define FERRITE_FLDIGI_SHIM_SYS_TIMERFD_H
#endif
