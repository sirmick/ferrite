/* Shim replacement for fldigi's <waterfall.h> (FLTK Fl_Group + WFdisp).
 * The modems touch `wf->Carrier/Reverse/USB/powerDensity/redraw_marker`
 * and configuration.h pulls a few enums/`palette`. Headless decode has
 * no waterfall, so this is a do-nothing `waterfall` whose getters
 * return benign values. Grow from compiler errors. */
#ifndef FERRITE_FLDIGI_SHIM_WATERFALL_H
#define FERRITE_FLDIGI_SHIM_WATERFALL_H

#include <complex>

typedef unsigned char uchar;

/* Minimal FLTK primitive types/constants. configuration.h uses these
 * as field defaults (fonts/colours); real fldigi gets them via
 * waterfall.h -> <FL/...>. Headless decode never draws — values just
 * need to exist with sane numbers. */
typedef int      Fl_Font;
typedef unsigned Fl_Color;
#define FL_HELVETICA    0
#define FL_COURIER      4
#define FL_TIMES        8
#define FL_NORMAL_SIZE  14
#define FL_FOREGROUND_COLOR 0
#define FL_BLACK        0
#define FL_RED          0x58000000u
#define FL_GREEN        0x3f000000u
#define FL_BLUE         0x10000000u
#define FL_YELLOW       0x67000000u
#define FL_WHITE        0xff000000u

struct RGB  { uchar R, G, B; };
struct RGBI { uchar R, G, B, I; };
extern RGB  palette[9];
extern RGBI mag2RGBI[256];

#define WF_FFTLEN     8192
#define WF_SAMPLERATE 8000
#define WF_BLOCKSIZE  512

typedef double wf_fft_type;
typedef std::complex<wf_fft_type> wf_cpx_type;

enum {
	WF_FFT_RECTANGULAR, WF_FFT_BLACKMAN, WF_FFT_HAMMING,
	WF_FFT_HANNING, WF_FFT_TRIANGULAR
};
enum WFmode  { WATERFALL, SPECTRUM, SCOPE, NUM_WF_MODES };
enum WFspeed { PAUSE = 0, FAST = 1, NORMAL = 2, SLOW = 4 };

extern void do_qsy(bool);

class waterfall {
public:
	waterfall(int = 0, int = 0, int = 0, int = 0, char * = 0) {}
	~waterfall() {}

	void USB(bool b) { usb = b; }
	bool USB() { return usb; }
	void Reverse(bool v) { reverse = v; }
	bool Reverse() { return reverse; }
	int  Carrier() { return carrierfreq; }
	void Carrier(int f) { carrierfreq = f; }
	void Mode(WFmode) {}
	void Bandwidth(int bw) { bandwidth = bw; }
	int  Bandwidth() { return bandwidth; }
	void rfcarrier(unsigned long long f) { rfc = f; }
	unsigned long long rfcarrier() { return rfc; }
	double powerDensity(double, double) { return 0.0; }
	double powerDensityMaximum(int, const int (*)[2]) const { return 0.0; }
	int  peakFreq(int, int) { return 0; }
	double Pwr(int) { return 0.0; }
	void redraw_marker() {}
	void movetocenter() {}
	void UI_select(bool) {}
	void set_XmtRcvBtn(bool) {}
	void opmode() {}

	/* Member enum (configuration.h names waterfall::WF_CARRIER). */
	enum { WF_NOP, WF_AFC_BW, WF_SIGNAL_SEARCH, WF_SQUELCH,
	       WF_CARRIER, WF_MODEM, WF_SCROLL };

private:
	bool usb = true;
	bool reverse = false;
	int  carrierfreq = 1000;
	int  bandwidth = 100;
	unsigned long long rfc = 0;
};

extern waterfall *wf;

#endif /* FERRITE_FLDIGI_SHIM_WATERFALL_H */
