/* Shim for <logbook.h> — the real one pulls lgbook.h (FLTK). navtex
   logs each decoded station as a QSO via QsoHelper; headless has no
   logbook, so QsoHelper is a no-op and the ADIF field ids are just
   distinct placeholders (Push is a no-op — values never used). */
#ifndef FERRITE_FLDIGI_SHIM_LOGBOOK_H
#define FERRITE_FLDIGI_SHIM_LOGBOOK_H
#include <string>
#include "globals.h"   /* trx_mode */

/* ADIF field ids navtex.cxx names. Arbitrary distinct values — the
   shim QsoHelper::Push discards them. */
enum {
	CALL = 1, GRIDSQUARE, NAME, QTH, COUNTRY,
	NOTES, XCHG1, SRX
};
#ifndef ADIF_EOL
#define ADIF_EOL "\r\n"
#endif

class QsoHelper {
public:
	QsoHelper(trx_mode = MODE_NULL) {}
	~QsoHelper() {}
	void Push(int, const std::string &) {}
};
#endif
