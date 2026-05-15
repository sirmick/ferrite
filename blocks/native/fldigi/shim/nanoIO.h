/* Shim for <nanoIO.h> — nanoIO CW/FSK TX keyer. RX never keys. */
#ifndef FERRITE_FLDIGI_SHIM_NANOIO_H
#define FERRITE_FLDIGI_SHIM_NANOIO_H
#include <string>
extern bool use_nanoIO;
extern bool nanoIO_isCW;
void set_nanoCW();
void nano_send_char(int c);
void nano_sendString(const std::string &);
void nano_PTT(int);
void set_nanoWPM(int);
void set_nano_dash2dot(float);
#endif
