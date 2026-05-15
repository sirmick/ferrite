/* Shim for <viewpsk.h> — fldigi multi-channel PSK browser (its .cxx
   isn't vendored). Single-channel decode goes via psk::rx_process ->
   put_rx_char; the viewer is a no-op. */
#ifndef FERRITE_FLDIGI_SHIM_VIEWPSK_H
#define FERRITE_FLDIGI_SHIM_VIEWPSK_H
#include "globals.h"
#include "complex.h"
class pskeval;
class viewpsk {
public:
	viewpsk(pskeval *, trx_mode) {}
	~viewpsk() {}
	void restart(trx_mode) {}
	int  rx_process(const double *, int) { return 0; }
	void rx_symbol(int, cmplx) {}
	void rx_bit(int, int) {}
	void rx_bit2(int, int) {}
	void rx_pskr(int, unsigned char) {}
	void rx_qpsk(int, int) {}
	void findsignal(int) {}
	void afc(int) {}
	bool is_valid_char(int &) { return false; }
	void clear() {}
	void clearch(int) {}
	int  get_freq(int) { return 0; }
};
#endif
