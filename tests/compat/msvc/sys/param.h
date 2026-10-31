/* sys/param.h for MSVC, which has none: the byte-order macros that the
 * unmodified chacha8.c of LMDB 1.0 (the cipher of its encryption tests,
 * tests/lmdb-c/1.0) takes from the BSD and glibc header. Every Windows
 * target is little-endian.
 *
 * Only on the include path of MSVC builds (interop/build.rs, with
 * tests/compat/msvc/unistd.h); MinGW-w64's <sys/param.h> defines the same
 * values.
 */
#ifndef RLMDB_MSVC_SYS_PARAM_H
#define RLMDB_MSVC_SYS_PARAM_H

#define LITTLE_ENDIAN	1234
#define BIG_ENDIAN	4321
#define BYTE_ORDER	LITTLE_ENDIAN

#endif /* RLMDB_MSVC_SYS_PARAM_H */
