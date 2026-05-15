/* Shim for <ICOMkeying.h> — rig CW keyer (TX). RX never keys. */
#ifndef FERRITE_FLDIGI_SHIM_ICOM_H
#define FERRITE_FLDIGI_SHIM_ICOM_H
extern bool use_ICOMkeyer;
extern int  ICOMwpm;
void set_ICOMkeyer();
void ICOMkeyer_send_char(int);
#endif
