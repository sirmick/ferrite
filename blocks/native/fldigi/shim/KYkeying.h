/* Shim for <KYkeying.h> — rig CW keyer (TX). RX never keys. */
#ifndef FERRITE_FLDIGI_SHIM_KY_H
#define FERRITE_FLDIGI_SHIM_KY_H
extern bool use_KYkeyer;
extern int  KYwpm;
void set_KYkeyer();
void KYkeyer_send_char(int);
#endif
