/* Shim for <view_cw.h> — fldigi multi-channel CW browser (FLTK).
   cw.cxx calls viewcw.restart()/rx_process() on the RX path; the real
   per-channel decode the host consumes is cw::rx_process -> put_rx_char,
   so a no-op viewer is correct. */
#ifndef FERRITE_FLDIGI_SHIM_VIEW_CW_H
#define FERRITE_FLDIGI_SHIM_VIEW_CW_H
class view_cw {
public:
	view_cw() {}
	void init(int = 0, double = 0) {}
	void restart() {}
	int  rx_process(const double *, int) { return 0; }
	void clear() {}
	void clearch(int) {}
	int  get_freq(int) { return 0; }
};
extern view_cw viewcw;
#endif
