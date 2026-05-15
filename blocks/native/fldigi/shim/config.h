/* Shim replacement for fldigi's autoconf-generated <config.h>.
 * fldigi modems #include <config.h> for version + HAVE_* feature
 * macros. We compile a curated RX-only subset with no autotools, so
 * provide the minimum the modem/filter sources actually probe. Grow
 * from compiler errors. */
#ifndef FERRITE_FLDIGI_SHIM_CONFIG_H
#define FERRITE_FLDIGI_SHIM_CONFIG_H

/* Autoconf's real config.h transitively pulled a wider libc include
 * chain than our minimal one; a few vendored sources (e.g.
 * timeops.cxx) rely on that for the mem/str functions without an
 * explicit include. config.h is included first by every fldigi TU, so
 * restore just the ubiquitous C headers here rather than patching
 * vendored files (keeps the vendor tree pristine for re-sync). */
#include <string.h>
#include <stdio.h>
#include <stdlib.h>

#define PACKAGE              "fldigi"
#define PACKAGE_NAME         "fldigi"
#define PACKAGE_TARNAME      "fldigi"
#define PACKAGE_VERSION      "4.2.11"
#define PACKAGE_STRING       "fldigi 4.2.11"
#define VERSION              "4.2.11"

/* std::bind is available (C++17 libstdc++/libc++); qrunner is shimmed
 * out but configuration/util may probe these. */
#define HAVE_STD_BIND        1
#define STD_BIND_NS          std

/* CRITICAL: without these, vendored timeops.cxx compiles its *own*
 * clock_gettime/gettimeofday fallbacks that shadow glibc's
 * process-wide and return values Rust's std rejects ("invalid
 * timestamp"). glibc has both — say so. */
#define HAVE_CLOCK_GETTIME   1
#define HAVE_GETTIMEOFDAY    1
#define HAVE_SEM_TIMEDWAIT   1
#define HAVE_NANOSLEEP       1

#define HAVE_STRING_H        1
#define HAVE_STDLIB_H        1
#define HAVE_MATH_H          1
#define HAVE_SYS_TIME_H      1
#define HAVE_DLFCN_H         0

/* glibc provides these; suppress fldigi util.h's fallback decls that
 * would otherwise clash with <string.h>. */
#define HAVE_STRCASESTR      1
#define HAVE_STRLWR          0
#define HAVE_STRUPR          0

/* fldigi's util.h provides these, but our shimmed header set breaks
 * some transitive include paths to it (e.g. globals.cxx via
 * strutil.h). config.h is included by every fldigi TU, so define them
 * here, #ifndef-guarded to defer to the real util.h when it is seen. */
#ifndef MAX
#  define MAX(a, b) (((a) > (b)) ? (a) : (b))
#endif
#ifndef MIN
#  define MIN(a, b) (((a) < (b)) ? (a) : (b))
#endif
#ifndef CLAMP
#  define CLAMP(x, low, high) (((x)>(high))?(high):(((x)<(low))?(low):(x)))
#endif
#ifndef powerof2
#  define powerof2(n) ((((n) - 1) & (n)) == 0)
#endif
#ifndef likely
#  define likely(x)   (x)
#endif
#ifndef unlikely
#  define unlikely(x) (x)
#endif

#endif /* FERRITE_FLDIGI_SHIM_CONFIG_H */
