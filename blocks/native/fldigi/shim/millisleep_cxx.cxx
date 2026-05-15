// fldigi declares MilliSleep/NanoSleep with TWO linkages across headers:
// util.h wraps them in `extern "C"` (rtty/fsk include path) while
// misc.h declares them plain C++ (cw include path). A single TU can't
// define the same name under both linkages, so the `extern "C"`
// definitions live in fldigi_shim.cxx and the C++-mangled ones live
// here. Distinct symbols (`MilliSleep` vs `_Z10MilliSleepl`); each
// caller binds whichever its header declared. Real impls — these are
// only on TX/keying/calibration paths, never reached in RX decode.

#include <ctime>

void MilliSleep(long msecs) {
	if (msecs <= 0) return;
	struct timespec ts;
	ts.tv_sec = msecs / 1000;
	ts.tv_nsec = (msecs % 1000) * 1000000L;
	nanosleep(&ts, 0);
}

void NanoSleep(double msecs) {
	if (msecs <= 0) return;
	struct timespec ts;
	ts.tv_sec = (time_t)(msecs / 1000.0);
	ts.tv_nsec = (long)((msecs - ts.tv_sec * 1000.0) * 1000000.0);
	nanosleep(&ts, 0);
}
