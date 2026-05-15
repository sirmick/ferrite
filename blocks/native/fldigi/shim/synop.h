/* Shim for <synop.h> — fldigi's SYNOP/SHIP/BUOY weather-report decoder
 * (a large WMO table subsystem in synop-src/). RTTY can feed it, but
 * only when progdefaults.Synop* are set (default off). rx_init() calls
 * SynopDB::Init / synop::setup / instance()->init() unconditionally,
 * so the stub must be a functional no-op (concrete instance, no pure
 * virtuals). The synop_callback ABI is preserved exactly so rtty.cxx's
 * `struct rtty_callback : public synop_callback` still compiles. */
#ifndef FERRITE_FLDIGI_SHIM_SYNOP_H
#define FERRITE_FLDIGI_SHIM_SYNOP_H

#include <string>
#include <cstddef>

class synop_callback {
public:
	virtual ~synop_callback() {}
	virtual bool interleaved(void) const { return true; }
	virtual void print(const char *, size_t, bool) const = 0;
	virtual bool log_adif(void) const = 0;
	virtual bool log_kml(void) const = 0;
};

class synop {
	static bool m_test_mode;
public:
	static const synop_callback *ptr_callback;

	template <class Callback>
	static void setup() {
		static const Callback cstCall = Callback();
		ptr_callback = &cstCall;
	}

	// Concrete no-op (real synop is abstract + defined in synop-src/).
	static synop *instance() {
		static synop inst;
		return &inst;
	}
	static void regex_usage(void) {}
	virtual ~synop() {}
	virtual void init() {}
	virtual void cleanup() {}
	virtual void add(char) {}
	virtual void flush(bool) {}
	virtual bool enabled(void) const { return false; }

	static bool GetTestMode(void) { return m_test_mode; }
	static void SetTestMode(bool t) { m_test_mode = t; }
};

struct SynopDB {
	static bool Init(const std::string &) { return false; }
	static const std::string &IndicatorToName(int);
	static const std::string  IndicatorToCoordinates(int);
	static const std::string &BuoyToName(const char *);
	static const std::string &ShipToName(const char *);
};

#endif /* FERRITE_FLDIGI_SHIM_SYNOP_H */
