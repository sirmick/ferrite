/* Shim for <main.h> (FLTK app entry/globals). Nothing on the RX decode
 * path needs it; symbols are added here only as compiler errors prove
 * a real dependency. */
#ifndef FERRITE_FLDIGI_SHIM_MAIN_H
#define FERRITE_FLDIGI_SHIM_MAIN_H
#include <string>
extern std::string HomeDir;
extern std::string PskMailDir;
extern std::string scDevice[2];
#endif
