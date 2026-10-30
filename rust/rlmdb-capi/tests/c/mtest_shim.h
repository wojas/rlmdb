/* Included (cc -include) into the unmodified upstream mtest*.c programs by
 * the tests, which also compile them with -Dtime=mtest_time:
 *
 * - they seed rand() with time(NULL); mtest_time() returns $MTEST_SEED
 *   instead, so a C-linked and a Rust-linked build do exactly the same;
 * - mtest6.c calls mdb_dkey(), which liblmdb only defines with MDB_DEBUG.
 */
#ifndef MTEST_SHIM_H
#define MTEST_SHIM_H

#include <time.h>
#include "lmdb.h"

time_t mtest_time(time_t *t);
char *mdb_dkey(MDB_val *key, char *buf);

#endif
