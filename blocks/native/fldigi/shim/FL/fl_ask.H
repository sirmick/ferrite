/* Shim for <FL/fl_ask.H> — FLTK modal dialogs. Headless decode never
   prompts; no-op everything navtex.cxx might call. */
#ifndef FERRITE_FLDIGI_SHIM_FL_FL_ASK_H
#define FERRITE_FLDIGI_SHIM_FL_FL_ASK_H
inline void fl_alert(const char *, ...) {}
inline void fl_message(const char *, ...) {}
inline int  fl_choice(const char *, const char *, const char *, const char *, ...) { return 0; }
inline int  fl_ask(const char *, ...) { return 0; }
inline const char *fl_input(const char *, const char * = 0, ...) { return 0; }
#endif
