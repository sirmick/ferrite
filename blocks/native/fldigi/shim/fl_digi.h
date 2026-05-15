/* Shim replacement for fldigi's <fl_digi.h>.
 *
 * The real header drags in ~30 FLTK widgets and the entire GUI. The
 * modem DSP only reaches it for a handful of *output* functions —
 * decoded characters, status lines, scope/metric. We declare exactly
 * those (signatures must match the vendored callers) and route them,
 * in fldigi_shim.cxx, into the C ABI callbacks the Rust block
 * registered. Everything else the GUI exposes here is intentionally
 * absent — nothing on the RX path uses it. */
#ifndef FERRITE_FLDIGI_SHIM_FL_DIGI_H
#define FERRITE_FLDIGI_SHIM_FL_DIGI_H

#include <string>
#include "globals.h"   /* trx_mode (vendored, FLTK-free) */
#include "complex.h"   /* cmplx (vendored, FLTK-free) — set_zdata */
#include "serial.h"    /* Cserial (vendored, FLTK-free) — rigio */

/* Misc utility macro the modems use (real: util.h, not on our path). */
#ifndef CLAMP
#define CLAMP(x, lo, hi) (((x)>(hi))?(hi):(((x)<(lo))?(lo):(x)))
#endif
#define IMAGE_WIDTH 3000          /* real: fldigi-config.h DEFAULT_IMAGE_WIDTH */
#ifndef PKGDATADIR
#define PKGDATADIR  "."           /* real: autoconf data dir; unused headless */
#endif

/* Text-attribute tags. Real fldigi puts these on FTextBase; the modem
 * code only ever names FTextBase::RECV as a default arg. */
struct FTextBase {
	enum { RECV, XMIT, CTRL, ALTR, NATTR, SKIP };
};

enum status_timeout {
	STATUS_CLEAR,
	STATUS_DIM,
	STATUS_KEEP
};

/* Decoded-character + status sink. fldigi_shim.cxx forwards these to
 * the active modem's registered callbacks. */
extern void put_rx_char(unsigned int data, int style = FTextBase::RECV);
extern void put_status(const char *msg, double timeout = 0.0,
                       status_timeout action = STATUS_CLEAR);
extern void put_Status1(const char *msg, double timeout = 0.0,
                        status_timeout action = STATUS_CLEAR);
extern void put_Status2(const char *msg, double timeout = 0.0,
                        status_timeout action = STATUS_CLEAR);
extern void put_MODEstatus(const char *fmt, ...);
extern void put_MODEstatus(trx_mode mode);
extern void put_freq(double frequency);

/* Scope / metric — no-op sinks (no waterfall in headless decode). */
extern void set_metric(double metric);
extern void set_scope(double *data, int len, bool autoscale = true);
struct Digiscope;                       /* fwd; full enum in digiscope.h */
extern void set_scope_mode(int md);     /* Digiscope::scope_mode is int-compatible */
extern void set_zdata(cmplx *, int);    /* cross-hair/phase scope feed */

/* ---- TX char queue (real: fl_digi.h). RX-only never calls these,
 * but rtty/fsk reference them at compile time. ---- */
#define GET_TX_CHAR_NODATA -1
#define GET_TX_CHAR_ETX    -3
extern int  get_tx_char();
extern void put_echo_char(unsigned int data, int style = FTextBase::XMIT);
extern void start_deadman();
extern void stop_deadman();

/* ---- TX/rig glue the RTTY modem links against on its transmit path
 * (FSK keying via flrig / WinKeyer / nanoIO / Navigator). Headless
 * RX-only never reaches these; stubbed so the unit links. ---- */
extern int  flrig_get_baud();
extern int  flrig_get_idles();
extern int  flrig_get_stopbits();
extern void flrig_fskio_send_text(std::string s);
extern void WKFSK_send_char(int c);
extern void nano_send_char(int c);
extern void Nav_send_char(int c);
extern bool use_Nav;
extern Cserial rigio;

/* dlgViewer (real: Viewer.h, FLTK). rtty probes dlgViewer->visible(). */
struct _shim_window { int visible() { return 0; } };
extern _shim_window *dlgViewer;

/* ---- modem.cxx (base modem) misc GUI/pskmail/CPS-test glue. All on
 * the TX / pskmail / noise-test paths, never reached RX-only. ---- */
#define PERFORM_CPS_TEST false
extern bool mailserver;
extern void put_Bandwidth(int bw);
extern void center_rxfilt_at_track();
extern void pskmail_notify_s2n(double, double, double);
struct _shim_valuator {
	double value() { return 0.0; }
	void value(double) {}
	void value(int) {}
	void value(const char *) {}
	void redraw() {}
};
extern _shim_valuator *noiseDB;
extern _shim_valuator *btnNoiseOn;

/* ---- CW (cw.cxx) glue: WPM display, scope axis, CWIO calibration UI,
 * flrig CW send, msec timer, FLTK Fl::awake. All TX/calibration/UI —
 * RX decode never reaches them. ---- */
extern void put_cwRcvWPM(double wpm);
extern void set_scope_xaxis_1(double y1);
extern void set_CWwpm();
extern void flrig_cwio_send_text(std::string);
extern unsigned long zmsec();
extern _shim_valuator *btn_cw_dtr_calibrate;
extern _shim_valuator *cwio_test_result;
extern _shim_valuator *out_CATkeying_compensation;
extern _shim_valuator *out_CATkeying_test_result;
extern _shim_valuator *cntCW_WPM;
extern _shim_valuator *sldrCWxmtWPM;
extern _shim_valuator *cntr_nanoCW_WPM;
struct Fl {
	static void awake() {}
	static void awake(void (*)(void *), void * = 0) {}
};

#endif /* FERRITE_FLDIGI_SHIM_FL_DIGI_H */
