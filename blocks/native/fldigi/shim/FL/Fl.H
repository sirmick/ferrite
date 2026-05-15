/* Shim for <FL/Fl.H>. olivia/contestia/etc #include it but never call
   Fl:: directly on the RX path (cross-thread UI goes via the no-op REQ
   macro). Empty — the `struct Fl` stub cw.cxx needs lives in
   shim/fl_digi.h. */
#ifndef FERRITE_FLDIGI_SHIM_FL_FL_H
#define FERRITE_FLDIGI_SHIM_FL_FL_H
#endif
