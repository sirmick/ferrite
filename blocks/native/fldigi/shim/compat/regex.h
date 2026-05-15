/* Shim for fldigi's bundled <compat/regex.h> — re.h includes it
   unconditionally. glibc ships POSIX regex (regex_t/regcomp/regexec/
   regfree), exactly what re.h's wrapper needs; just use it. */
#ifndef FERRITE_FLDIGI_SHIM_COMPAT_REGEX_H
#define FERRITE_FLDIGI_SHIM_COMPAT_REGEX_H
#include <regex.h>
#endif
