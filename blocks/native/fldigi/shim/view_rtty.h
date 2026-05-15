/* Shim for <view_rtty.h> — fldigi's multi-channel RTTY browser (FLTK,
 * pulls Viewer.h). The single-channel decode path doesn't need it;
 * rtty.cxx only ever new's one and calls restart()/rx_process(), both
 * no-ops here (the real per-channel decode the block consumes comes
 * through put_rx_char). */
#ifndef FERRITE_FLDIGI_SHIM_VIEW_RTTY_H
#define FERRITE_FLDIGI_SHIM_VIEW_RTTY_H

#include "globals.h"   /* trx_mode (vendored, FLTK-free) */

class view_rtty {
public:
	view_rtty(trx_mode) {}
	~view_rtty() {}
	void restart() {}
	int  rx_process(const double *, int) { return 0; }
	void clear() {}
	void clearch(int) {}
	int  get_freq(int) { return 0; }
};

#endif /* FERRITE_FLDIGI_SHIM_VIEW_RTTY_H */
