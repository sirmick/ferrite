// fldigi_shim.cxx — the single C++ shim that satisfies fldigi's
// link-time coupling for headless RX-only decode.
//
// It provides, in one place:
//   * the C ABI (fldigi_modem_create/rx/set_param/destroy) the Rust
//     crate's src/lib.rs declares;
//   * the global state fldigi expects (active_modem, trx_state,
//     progdefaults, progStatus, wf, …);
//   * implementations of fldigi's output sinks (put_rx_char,
//     put_status, set_scope, …) routed to the registered callbacks.
//
// The runtime is single-threaded and drives one modem per block, so
// there is no locking — `g_active` names the modem whose rx_process()
// is currently running, set around each fldigi_modem_rx call.

#include "configuration.h"   // vendored: struct configuration + CONFIG_LIST
#include "status.h"          // vendored: struct status
#include "globals.h"         // vendored: state_t, trx_mode, mode_info
#include "modem.h"           // vendored: class modem (+ static members)
#include "rtty.h"            // vendored: class rtty
#include "cw.h"              // vendored: class cw
#include "psk.h"             // vendored: class psk
#include "mt63.h"            // vendored: class mt63
#include "throb.h"           // vendored: class throb
#include "dominoex.h"        // vendored: class dominoex
#include "olivia.h"          // vendored: class olivia
#include "contestia.h"       // vendored: class contestia
#include "navtex.h"          // vendored: class navtex
#include "ptt.h"             // shim
#include "winkeyer.h"        // shim
#include "nanoIO.h"          // shim
#include "KYkeying.h"        // shim
#include "ICOMkeying.h"      // shim
#include "YAESUkeying.h"     // shim
#include "view_cw.h"         // shim
#include "waterfall.h"       // shim
#include "digiscope.h"       // shim
#include "audio_alert.h"     // shim
#include "test_signal.h"     // shim
#include "synop.h"           // shim
#include "debug.h"           // shim
#include "fl_digi.h"         // shim: declares the sinks we define below

#include <string>
#include <vector>
#include <cstdarg>

// ---------------------------------------------------------------------
// progdefaults, initialised with fldigi's own defaults. This is the
// exact mechanism from misc/configuration.cxx: redefine ELEM_ to emit
// each field's default value, then aggregate-init from CONFIG_LIST.
// ---------------------------------------------------------------------
#define ELEM_PROGDEFAULTS(type_, var_, tag_, doc_, ...) __VA_ARGS__,
#undef  ELEM_
#define ELEM_ ELEM_PROGDEFAULTS
configuration progdefaults = { CONFIG_LIST };

// ---------------------------------------------------------------------
// Globals fldigi links against. progStatus value-inited (zeros PODs,
// empties strings); RTTY-relevant fields set in create().
// ---------------------------------------------------------------------
status        progStatus = status();
modem        *active_modem = 0;
state_t       trx_state = STATE_RX;
bool          bHistory = false;
bool          bHighSpeed = false;
bool          rx_only = true;
SoundBase    *RXscard = 0;
SoundBase    *TXscard = 0;
// wf is dereferenced unconditionally on the rx path (Carrier/Reverse/
// USB/powerDensity). Point at a static no-op waterfall, never null.
static waterfall s_wf;
waterfall    *wf = &s_wf;
RGB           palette[9] = {};
RGBI          mag2RGBI[256] = {};
cAudioAlerts *audio_alert = 0;
test_signal_dialog *test_signal_window = 0;
debug::level_e debug::level = debug::INFO_LEVEL;
unsigned int   debug::mask = 0;
std::string    HomeDir, PskMailDir;
std::string    scDevice[2];

// NB: modem::frequency / tx_frequency / freqlock / tx_sample_count /
// tx_sample_rate / XMLRPC_CPS_TEST are defined by vendored modem.cxx —
// do not redefine here (duplicate-symbol at link).

// ---------------------------------------------------------------------
// Pull/drain bridge. fldigi's output sinks append into the active
// modem's internal buffers; the host *drains* them via the ABI after
// each rx. No callbacks: a callback's function pointer is callable
// across the native static link but NOT across the two separate wasm
// modules of the Emscripten bridge (distinct tables/memories). Pull is
// the one shape that works identically native and bridged, and it
// mirrors what the Rust wrapper already did (accumulate → take_*).
// ---------------------------------------------------------------------
namespace {
struct Bridge {
	modem *m = 0;
	std::string text;            // decoded chars (put_rx_char)
	std::string status;          // status lines, '\n'-joined
	std::vector<float> scope;    // most recent scope frame
	int scope_mode = 0;          // fldigi Digiscope::scope_mode
};
Bridge *g_active = 0;
std::vector<double> g_rxbuf;
} // namespace

// ---------------------------------------------------------------------
// fldigi output sinks (declared in shim/fl_digi.h).
// ---------------------------------------------------------------------
void put_rx_char(unsigned int data, int /*style*/) {
	if (!g_active) return;
	// fldigi emits already-decoded code points; modem text is 7/8-bit.
	// Encode UTF-8 so the Rust side gets valid str (matches the old
	// callback's char::from_u32 path).
	unsigned int c = data;
	if (c < 0x80) {
		g_active->text.push_back((char)c);
	} else if (c < 0x800) {
		g_active->text.push_back((char)(0xC0 | (c >> 6)));
		g_active->text.push_back((char)(0x80 | (c & 0x3F)));
	} else {
		g_active->text.push_back((char)(0xE0 | (c >> 12)));
		g_active->text.push_back((char)(0x80 | ((c >> 6) & 0x3F)));
		g_active->text.push_back((char)(0x80 | (c & 0x3F)));
	}
}
static void append_status(const char *msg) {
	if (g_active && msg && *msg) {
		g_active->status.append(msg);
		g_active->status.push_back('\n');
	}
}
void put_status(const char *msg, double, status_timeout)  { append_status(msg); }
void put_Status1(const char *msg, double, status_timeout) { append_status(msg); }
void put_Status2(const char *, double, status_timeout) {}
void put_MODEstatus(const char *, ...) {}
void put_MODEstatus(trx_mode) {}
void put_freq(double) {}
void set_metric(double) {}
void set_scope(double *data, int len, bool) {
	if (g_active && data && len > 0)
		g_active->scope.assign(data, data + len);  // keep latest frame
}
void set_scope_mode(int md) {
	if (g_active) g_active->scope_mode = md;
}
void set_zdata(cmplx *, int) {}
void do_qsy(bool) {}

// TX char queue + TX/rig glue: RX-only never invokes these (rtty's
// transmit path), but the unit must link. Globals fldigi expects.
int  get_tx_char() { return GET_TX_CHAR_NODATA; }
void put_echo_char(unsigned int, int) {}
void start_deadman() {}
void stop_deadman() {}
int  flrig_get_baud() { return 0; }
int  flrig_get_idles() { return 0; }
int  flrig_get_stopbits() { return 0; }
void flrig_fskio_send_text(std::string) {}
void WKFSK_send_char(int) {}
void nano_send_char(int) {}
void Nav_send_char(int) {}
bool use_Nav = false;
Cserial rigio;
static _shim_window s_dlgViewer;
_shim_window *dlgViewer = &s_dlgViewer;  // rtty.cxx derefs unconditionally
bool mailserver = false;
bool mailclient = false;
void set_phase(double, double, bool) {}
void set_video(double *, int, bool) {}
void put_sec_char(char) {}  // secondary channel — primary decode via put_rx_char
void start_tx() {}          // navtex TX path — never reached RX-only
void put_Bandwidth(int) {}
void center_rxfilt_at_track() {}
void pskmail_notify_s2n(double, double, double) {}
static _shim_valuator s_noiseDB, s_btnNoiseOn;
_shim_valuator *noiseDB = &s_noiseDB;
_shim_valuator *btnNoiseOn = &s_btnNoiseOn;
void trx_xmit_wfall_queue(int, const double *, size_t) {}

// fldigi defines MilliSleep/NanoSleep in main.cxx (not vendored).
// util.h declares them inside `extern "C"`, so match that linkage.
// Real impls — rtty/fsk TX timing uses them; correct if ever reached.
#include <ctime>
extern "C" void MilliSleep(long msecs) {
	if (msecs <= 0) return;
	struct timespec ts;
	ts.tv_sec  = msecs / 1000;
	ts.tv_nsec = (msecs % 1000) * 1000000L;
	nanosleep(&ts, 0);
}
extern "C" void NanoSleep(double msecs) {
	if (msecs <= 0) return;
	struct timespec ts;
	ts.tv_sec  = (time_t)(msecs / 1000.0);
	ts.tv_nsec = (long)((msecs - ts.tv_sec * 1000.0) * 1000000.0);
	nanosleep(&ts, 0);
}

// synop shim statics (declared in shim/synop.h).
const synop_callback *synop::ptr_callback = 0;
bool synop::m_test_mode = false;

// fldigi's global modem-pointer registry (modem.h `extern modem *x;`).
// fldigi defines these in its mode-init code we don't vendor; modem.cxx
// references them. RX-only never dereferences any but `active_modem`,
// so null definitions satisfy the link.
// (modem registry: all 172 `extern modem *x;` are defined by
// vendored modem.cxx itself — nothing to define here.)

// CW (Morse) TX-keying glue: cw.cxx links these on its transmit path
// (WinKeyer / nanoIO / rig keyers / PTT / multi-channel viewer). RX-
// only decode never reaches them — no-op definitions satisfy the link.
static PTT s_push2talk;
PTT *push2talk = &s_push2talk;
view_cw viewcw;
bool use_nanoIO = false;
bool nanoIO_isCW = false;
bool WK_online = false;
bool use_KYkeyer = false;   int KYwpm = 22;
bool use_ICOMkeyer = false; int ICOMwpm = 22;
bool use_FTkeyer = false;   int FTwpm = 22;
int  WK_send_char(int) { return 0; }
void WK_set_wpm() {}
void WK_set_comp() {}
void WK_reset_timing() {}
void set_nanoCW() {}
// nano_send_char(int) already defined above (RTTY-era stub).
void nano_sendString(const std::string &) {}
void nano_PTT(int) {}
void set_nanoWPM(int) {}
void set_nano_dash2dot(float) {}
void set_KYkeyer() {}    void KYkeyer_send_char(int) {}
void set_ICOMkeyer() {}  void ICOMkeyer_send_char(int) {}
void set_FTkeyer() {}    void FTkeyer_send_char(int) {}

// CW display / scope / flrig / timer glue (decl in shim/fl_digi.h).
void put_cwRcvWPM(double) {}
void set_scope_xaxis_1(double) {}
void set_CWwpm() {}
void flrig_cwio_send_text(std::string) {}
unsigned long zmsec() { return 0; }
static _shim_valuator s_cwwid;
_shim_valuator *btn_cw_dtr_calibrate = &s_cwwid;
_shim_valuator *cwio_test_result = &s_cwwid;
_shim_valuator *out_CATkeying_compensation = &s_cwwid;
_shim_valuator *out_CATkeying_test_result = &s_cwwid;
_shim_valuator *cntCW_WPM = &s_cwwid;
_shim_valuator *sldrCWxmtWPM = &s_cwwid;
_shim_valuator *cntr_nanoCW_WPM = &s_cwwid;
_shim_valuator *btn_imd_on = &s_cwwid;
_shim_valuator *xmtimd = &s_cwwid;

// ---------------------------------------------------------------------
// Mode registry: id string -> constructed modem.
// ---------------------------------------------------------------------
static modem *make_modem(const std::string &id) {
	if (id == "rtty" || id == "rtty45" || id == "rtty50" ||
	    id == "rtty75" || id == "rtty100")
		return new rtty(MODE_RTTY);
	if (id == "cw" || id == "morse")
		return new cw();
	if (id == "psk31")  return new psk(MODE_PSK31);
	if (id == "psk63")  return new psk(MODE_PSK63);
	if (id == "psk125") return new psk(MODE_PSK125);
	if (id == "mt63-500S")  return new mt63(MODE_MT63_500S);
	if (id == "mt63-500L")  return new mt63(MODE_MT63_500L);
	if (id == "mt63-1000S") return new mt63(MODE_MT63_1000S);
	if (id == "mt63-1000L") return new mt63(MODE_MT63_1000L);
	if (id == "mt63-2000S") return new mt63(MODE_MT63_2000S);
	if (id == "mt63-2000L") return new mt63(MODE_MT63_2000L);
	if (id == "throb1")  return new throb(MODE_THROB1);
	if (id == "throb2")  return new throb(MODE_THROB2);
	if (id == "throb4")  return new throb(MODE_THROB4);
	if (id == "throbx1") return new throb(MODE_THROBX1);
	if (id == "throbx2") return new throb(MODE_THROBX2);
	if (id == "throbx4") return new throb(MODE_THROBX4);
	if (id == "dominoex4")  return new dominoex(MODE_DOMINOEX4);
	if (id == "dominoex8")  return new dominoex(MODE_DOMINOEX8);
	if (id == "dominoex11") return new dominoex(MODE_DOMINOEX11);
	if (id == "dominoex16") return new dominoex(MODE_DOMINOEX16);
	if (id == "dominoex22") return new dominoex(MODE_DOMINOEX22);
	if (id == "dominoex44") return new dominoex(MODE_DOMINOEX44);
	if (id == "olivia")        return new olivia(MODE_OLIVIA);
	if (id == "olivia-8-500")  return new olivia(MODE_OLIVIA_8_500);
	if (id == "olivia-16-500") return new olivia(MODE_OLIVIA_16_500);
	if (id == "olivia-32-1000") return new olivia(MODE_OLIVIA_32_1000);
	if (id == "contestia")        return new contestia(MODE_CONTESTIA);
	if (id == "contestia-8-250")  return new contestia(MODE_CONTESTIA_8_250);
	if (id == "contestia-8-500")  return new contestia(MODE_CONTESTIA_8_500);
	if (id == "contestia-16-500") return new contestia(MODE_CONTESTIA_16_500);
	if (id == "navtex") return new navtex(MODE_NAVTEX);
	if (id == "sitorb") return new navtex(MODE_SITORB);
	return 0;
}

// ---------------------------------------------------------------------
// C ABI (matches blocks/native/fldigi/src/lib.rs).
// ---------------------------------------------------------------------
struct fldigi_modem { Bridge br; };

extern "C" {

fldigi_modem *fldigi_modem_create(const char *mode, int sample_rate) {
	modem *m = make_modem(mode ? std::string(mode) : std::string());
	if (!m) return 0;

	fldigi_modem *h = new fldigi_modem();
	h->br.m = m;

	g_active = &h->br;
	active_modem = m;
	m->set_samplerate(sample_rate);
	m->init();
	m->rx_init();
	return h;
}

int fldigi_modem_rx(fldigi_modem *h, const float *audio, int n) {
	if (!h || !audio || n <= 0) return -1;
	g_active = &h->br;
	active_modem = h->br.m;
	g_rxbuf.assign(audio, audio + n);
	return h->br.m->rx_process(g_rxbuf.data(), n);
}

// Drain accumulated decoded text into `out` (up to `cap` bytes);
// returns bytes written and removes them from the buffer. Call until
// it returns < cap (or 0). Same contract for status.
int fldigi_modem_drain_text(fldigi_modem *h, char *out, int cap) {
	if (!h || !out || cap <= 0) return 0;
	std::string &s = h->br.text;
	int n = (int)s.size();
	if (n > cap) n = cap;
	if (n > 0) {
		std::memcpy(out, s.data(), (size_t)n);
		s.erase(0, (size_t)n);
	}
	return n;
}

int fldigi_modem_drain_status(fldigi_modem *h, char *out, int cap) {
	if (!h || !out || cap <= 0) return 0;
	std::string &s = h->br.status;
	int n = (int)s.size();
	if (n > cap) n = cap;
	if (n > 0) {
		std::memcpy(out, s.data(), (size_t)n);
		s.erase(0, (size_t)n);
	}
	return n;
}

// Drain the most recent scope frame: up to `cap` floats into `out`,
// the fldigi Digiscope::scope_mode into `*mode`. Returns float count
// (0 if no new frame). Coalescing to the latest frame is correct for
// a tuning scope. (Phase 3+ frontend widget consumes this.)
int fldigi_modem_drain_scope(fldigi_modem *h, float *out, int cap, int *mode) {
	if (!h || !out || cap <= 0) return 0;
	std::vector<float> &v = h->br.scope;
	if (v.empty()) return 0;
	int n = (int)v.size();
	if (n > cap) n = cap;
	std::memcpy(out, v.data(), (size_t)n * sizeof(float));
	if (mode) *mode = h->br.scope_mode;
	v.clear();
	return n;
}

// Image plane reserved for the visual modes (Phase 3). ABI fixed now
// so native + the Emscripten bridge stay in lockstep; always 0 here.
int fldigi_modem_drain_image(fldigi_modem *, unsigned char *, int,
                             int *, int *) {
	return 0;
}

void fldigi_modem_set_param(fldigi_modem *h, const char *key, double v) {
	if (!h || !key) return;
	std::string k(key);
	// Operational RTTY knobs; index conventions match fldigi's config.
	if      (k == "rtty_baud")    progdefaults.rtty_baud   = (int)v;
	else if (k == "rtty_bits")    progdefaults.rtty_bits   = (int)v;
	else if (k == "rtty_shift")   progdefaults.rtty_shift  = (int)v;
	else if (k == "rtty_parity")  progdefaults.rtty_parity = (int)v;
	else if (k == "rtty_stop")    progdefaults.rtty_stop   = (int)v;
	else if (k == "rtty_reverse") progdefaults.rtty_reverse = (v != 0.0);
	else if (k == "afc")          progStatus.afconoff      = (v != 0.0);
	if (h->br.m) h->br.m->restart();
}

void fldigi_modem_destroy(fldigi_modem *h) {
	if (!h) return;
	if (g_active == &h->br) g_active = 0;
	if (active_modem == h->br.m) active_modem = 0;
	delete h->br.m;
	delete h;
}

} // extern "C"
