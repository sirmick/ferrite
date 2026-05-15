/* Shim replacement for fldigi's <digiscope.h> (an FLTK Fl_Widget).
 * modem.h holds a `Digiscope::scope_mode scopemode;` member and the
 * modems name `Digiscope::RTTY` / `Digiscope::XHAIRS` etc. We need the
 * enum and a do-nothing class — no drawing in headless decode. */
#ifndef FERRITE_FLDIGI_SHIM_DIGISCOPE_H
#define FERRITE_FLDIGI_SHIM_DIGISCOPE_H

#define MAX_ZLEN  1024
#define NUM_GRIDS 100

class Digiscope {
public:
	enum scope_mode {
		SCOPE, PHASE, PHASE1, PHASE2, PHASE3,
		RTTY, XHAIRS, WWV, DOMDATA, DOMWF, BLANK
	};
	Digiscope(int = 0, int = 0, int = 0, int = 0, const char * = 0) {}
	~Digiscope() {}
	void mode(scope_mode) {}
	scope_mode mode() { return _mode; }
	void data(double * = 0, int = 0, bool = true) {}
	void phase(double = 0, double = 0, bool = false) {}
	void scopedata(double * = 0, int = 0) {}
	void clear() {}
	void zoom(int = 0) {}
	void redraw() {}
	void redraw_marker() {}
private:
	scope_mode _mode = SCOPE;
};

#endif /* FERRITE_FLDIGI_SHIM_DIGISCOPE_H */
