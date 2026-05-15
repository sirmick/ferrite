/* Shim for <ptt.h> — fldigi PTT/keyline (TX). cw.cxx only touches
   push2talk->serPort on the transmit path (RX-dead). */
#ifndef FERRITE_FLDIGI_SHIM_PTT_H
#define FERRITE_FLDIGI_SHIM_PTT_H
#include "serial.h"
class PTT {
public:
	Cserial serPort;
	PTT() {}
	void set(bool) {}
	void reset(int = 0) {}
};
extern PTT *push2talk;
#endif
