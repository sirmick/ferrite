// Standalone freqdem instantiation — liquid's upstream bundles the
// freqdem implementation inside `modem/src/modemcf.c`, which in turn
// drags in the full digital-modem zoo (PSK, QAM, APSK, FSK, GMSK, …)
// via a dozen other `*.proto.c` includes. We only want the analog FM
// demodulator, so we include the single proto file here under the
// same macro plumbing the vendor uses, and link the resulting
// `freqdem_*` symbols alongside `ampmodem.c`.

#include <math.h>
#include <stdlib.h>
#include "liquid.internal.h"

#define T         float
#define TC        liquid_float_complex
#define EXTENSION ""
#define FREQDEM(name) LIQUID_CONCAT(freqdem, name)

#include "src/modem/src/freqdem.proto.c"
