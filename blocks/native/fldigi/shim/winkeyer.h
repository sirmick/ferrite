/* Shim for <winkeyer.h> — WinKeyer CW TX. RX never keys. */
#ifndef FERRITE_FLDIGI_SHIM_WINKEYER_H
#define FERRITE_FLDIGI_SHIM_WINKEYER_H
extern bool WK_online;
int  WK_send_char(int c);
void WK_set_wpm();
void WK_set_comp();
void WK_reset_timing();
#endif
