/* Shim for <FTextRXTX.h> — FLTK rx/tx text widgets. Headless decode
   emits via put_rx_char (shim/fl_digi.h); these are never drawn. */
#ifndef FERRITE_FLDIGI_SHIM_FTEXTRXTX_H
#define FERRITE_FLDIGI_SHIM_FTEXTRXTX_H
#include "fl_digi.h"
class FTextView {
public:
	void add(unsigned int, int = 0) {}
	void add(const char *, int = 0) {}
};
class FTextRX : public FTextView {
public:
	FTextRX(int = 0, int = 0, int = 0, int = 0, const char * = 0) {}
};
class FTextTX : public FTextView {
public:
	FTextTX(int = 0, int = 0, int = 0, int = 0, const char * = 0) {}
};
#endif
