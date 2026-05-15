/* Shim for <YAESUkeying.h> — rig CW keyer (TX). RX never keys. */
#ifndef FERRITE_FLDIGI_SHIM_FT_H
#define FERRITE_FLDIGI_SHIM_FT_H
extern bool use_FTkeyer;
extern int  FTwpm;
void set_FTkeyer();
void FTkeyer_send_char(int);
#endif
