/* Shim replacement for fldigi's <qrunner.h>.
 * Real qrunner marshals callbacks onto the FLTK main thread. Ferrite's
 * runtime is single-threaded and headless, so REQ(...) just *is* the
 * call — but on the RX text path nothing it queues matters, so the
 * cheapest correct shim is a no-op. Image/scope output is captured via
 * the dedicated shim hooks, not REQ. */
#ifndef FERRITE_FLDIGI_SHIM_QRUNNER_H
#define FERRITE_FLDIGI_SHIM_QRUNNER_H

#define REQ(...)        ((void)0)
#define REQ_DROP(...)   ((void)0)
#define REQ_SYNC(...)   ((void)0)
#define REQ_FLUSH(...)  ((void)0)
#define REQ_ASYNC(...)  ((void)0)
#define QRUNNER_DROP(...) ((void)0)

#define GET_THREAD_ID() (0)
#define FLMAIN_TID      (0)
#define TRX_TID         (1)
#define ENSURE_THREAD(x)      do {} while (0)
#define ENSURE_NOT_THREAD(x)  do {} while (0)

#endif /* FERRITE_FLDIGI_SHIM_QRUNNER_H */
