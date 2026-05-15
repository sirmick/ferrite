/* Shim replacement for fldigi's <debug.h>.
 * LOG_* go nowhere in headless decode (the Rust side owns tracing).
 * Keep the `debug` enums — some sources name debug::LOG_MODEM etc. */
#ifndef FERRITE_FLDIGI_SHIM_DEBUG_H
#define FERRITE_FLDIGI_SHIM_DEBUG_H

class debug {
public:
	enum level_e {
		QUIET_LEVEL, ERROR_LEVEL, WARN_LEVEL, INFO_LEVEL,
		VERBOSE_LEVEL, DEBUG_LEVEL, LOG_NLEVELS
	};
	enum source_e {
		LOG_ARQCONTROL = 1 << 0,  LOG_AUDIO     = 1 << 1,
		LOG_MODEM      = 1 << 2,  LOG_RIGCONTROL= 1 << 3,
		LOG_RPC_CLIENT = 1 << 4,  LOG_RPC_SERVER= 1 << 5,
		LOG_SPOTTER    = 1 << 6,  LOG_DATASOURCES=1 << 7,
		LOG_SYNOP      = 1 << 8,  LOG_KML       = 1 << 9,
		LOG_KISSCONTROL= 1 << 10, LOG_MACLOGGER = 1 << 11,
		LOG_FD         = 1 << 12, LOG_N3FJP     = 1 << 13,
		LOG_OTHER      = 1 << 14
	};
	static level_e level;
	static unsigned int mask;
};

#define LOG(...)          do {} while (0)
#define LOG_DEBUG(...)    do {} while (0)
#define LOG_VERBOSE(...)  do {} while (0)
#define LOG_INFO(...)     do {} while (0)
#define LOG_WARN(...)     do {} while (0)
#define LOG_ERROR(...)    do {} while (0)
#define LOG_HD(...)       do {} while (0)
#define LOG_HEX(...)      do {} while (0)
#define LOG_PERROR(...)   do {} while (0)
#define LOG_FILE_SOURCE(source__)
#define LOG_SET_SOURCE(source__) do {} while (0)

#endif /* FERRITE_FLDIGI_SHIM_DEBUG_H */
