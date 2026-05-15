/* Shim for <lookupcall.h>. The real header drives QRZ/HamQTH callsign
 * web lookups (GUI/network). configuration.h includes it and uses its
 * enums (and, in real fldigi, transitively-visible config defaults) as
 * ELEM_ field defaults — so this shim must surface exactly those
 * symbols. No lookup behaviour on the RX decode path. */
#ifndef FERRITE_FLDIGI_SHIM_LOOKUPCALL_H
#define FERRITE_FLDIGI_SHIM_LOOKUPCALL_H

enum qrz_xmlquery_t {
	QRZXML_EXIT = -1, QRZXMLNONE,
	QRZNET, QRZCD, HAMCALLNET, CALLOOK, HAMQTH
};
enum qrz_webquery_t {
	QRZWEB_EXIT = -1, QRZWEBNONE,
	QRZHTML, HAMCALLHTML, HAMQTHHTML, CALLOOKHTML
};

/* Sound backend index enum (real: soundconf.h) — config default. */
enum { SND_IDX_UNKNOWN = -1, SND_IDX_OSS, SND_IDX_PORT,
       SND_IDX_PULSE, SND_IDX_NULL, SND_IDX_END };

/* libsamplerate quality (real: <samplerate.h>) — config default. */
#define SRC_SINC_BEST_QUALITY   0
#define SRC_SINC_MEDIUM_QUALITY 1
#define SRC_SINC_FASTEST        2
#define SRC_ZERO_ORDER_HOLD     3
#define SRC_LINEAR              4

/* Network endpoint defaults (real: data_io.h) — config defaults. */
#define DEFAULT_ARQ_IP_ADDRESS    "127.0.0.1"
#define DEFAULT_ARQ_IP_PORT       "7322"
#define DEFAULT_KISS_IP_ADDRESS   "127.0.0.1"
#define DEFAULT_KISS_IP_IO_PORT   "7342"
#define DEFAULT_KISS_IP_OUT_PORT  "7343"
#define DEFAULT_XMLPRC_IP_ADDRESS "127.0.0.1"
#define DEFAULT_XMLRPC_IP_PORT    "7362"
#define DEFAULT_FLRIG_IP_ADDRESS  "127.0.0.1"
#define DEFAULT_FLRIG_IP_PORT     "12345"

#endif /* FERRITE_FLDIGI_SHIM_LOOKUPCALL_H */
