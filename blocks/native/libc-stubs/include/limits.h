/* Minimal limits.h stub for wasm32-unknown-unknown.
 *
 * clang's built-in <limits.h> is OS-neutral and ships with the
 * compiler — but `-nostdlibinc` strips the built-in include path too.
 * Re-declaring the standard limits here keeps the C99 minimums; any
 * 64-bit-specific extension a vendor reaches for needs to be added
 * explicitly.
 */

#ifndef FERRITE_LIBC_STUBS_LIMITS_H
#define FERRITE_LIBC_STUBS_LIMITS_H

#define CHAR_BIT  8

#define SCHAR_MIN (-128)
#define SCHAR_MAX  127
#define UCHAR_MAX  255
#define CHAR_MIN  SCHAR_MIN
#define CHAR_MAX  SCHAR_MAX

#define SHRT_MIN  (-32768)
#define SHRT_MAX   32767
#define USHRT_MAX  65535

#define INT_MIN   (-2147483647 - 1)
#define INT_MAX    2147483647
#define UINT_MAX   4294967295U

#define LONG_MIN  (-9223372036854775807L - 1)
#define LONG_MAX   9223372036854775807L
#define ULONG_MAX  18446744073709551615UL

#define LLONG_MIN (-9223372036854775807LL - 1)
#define LLONG_MAX  9223372036854775807LL
#define ULLONG_MAX 18446744073709551615ULL

/* Path / name length — referenced by a few rtl_433 device decoders
 * that build file-name buffers (capture / dumper paths that never
 * fire in our use). 4096 matches Linux. */
#define PATH_MAX 4096
#define NAME_MAX 255

#endif /* FERRITE_LIBC_STUBS_LIMITS_H */
