/* Included first (cc -include, cl /FI) into the unmodified upstream
 * mtest*.c programs by the tests, and by mtest_shim.c:
 *
 * - they seed rand() with time(NULL); here `time` becomes mtest_time(),
 *   which returns $MTEST_SEED instead, so that a C-linked and a
 *   Rust-linked build do exactly the same. rand() and srand() become
 *   mtest_rand() and mtest_srand(): on OpenBSD rand() is random unless
 *   seeded with srand_deterministic(), and mtest4 never seeds it;
 * - mtest6.c calls mdb_dkey(), which liblmdb only defines with MDB_DEBUG.
 *
 * <time.h> and <stdlib.h> come first and the macros after them: MSVC's C
 * runtime (the UCRT) defines time() in <time.h> as an inline function,
 * which a -Dtime=... on the command line would rename into a second
 * definition of mtest_time(), and declares rand() and srand() as imports,
 * which the macros would turn into imports of our functions. The
 * programs' own #include of these headers then adds nothing.
 */
#ifndef MTEST_SHIM_H
#define MTEST_SHIM_H

#include <stdlib.h>
#include <time.h>
#include "lmdb.h"

time_t mtest_time(time_t *t);
void mtest_srand(unsigned seed);
int mtest_rand(void);
char *mdb_dkey(MDB_val *key, char *buf);

#define time	mtest_time
#define srand	mtest_srand
#define rand	mtest_rand

#endif
