/* Shim for <pskeval.h> — PSK signal-evaluation helper (its .cxx isn't
   vendored). psk.cxx new's one and calls setbw/sigdensity/sigpeak on
   the RX path; a no-op satisfies link (real metric drives only the
   GUI signal browser, absent headless). */
#ifndef FERRITE_FLDIGI_SHIM_PSKEVAL_H
#define FERRITE_FLDIGI_SHIM_PSKEVAL_H
class pskeval {
public:
	pskeval() {}
	~pskeval() {}
	void   clear() {}
	void   setbw(double) {}
	void   sigdensity() {}
	double sigpeak(int &, int, int) { return 0.0; }
	double peak(int &, int, int, double) { return 0.0; }
	double power(int, int) { return 0.0; }
};
#endif
