/* Included first (cc -include, cl /FI) into the unmodified upstream
 * mtest*.c programs by the tests, and by mtest_shim.c:
 *
 * - they seed rand() with time(NULL); here `time` becomes mtest_time(),
 *   which returns $MTEST_SEED instead, so that a C-linked and a
 *   Rust-linked build do exactly the same;
 * - mtest6.c calls mdb_dkey(), which liblmdb only defines with MDB_DEBUG.
 *
 * <time.h> comes first and the macro after it: MSVC's C runtime (the UCRT)
 * defines time() in <time.h> as an inline function, which a -Dtime=...
 * on the command line would rename into a second definition of
 * mtest_time(). The programs' own #include <time.h> then adds nothing.
 */
#ifndef MTEST_SHIM_H
#define MTEST_SHIM_H

#include <time.h>
#include "lmdb.h"

time_t mtest_time(time_t *t);
char *mdb_dkey(MDB_val *key, char *buf);

#define time	mtest_time

#endif
