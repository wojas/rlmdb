/* See mtest_shim.h. Compiled with the same -Dtime=mtest_time. */
#include <stdio.h>
#include <stdlib.h>
#include "mtest_shim.h"

/* The rand() seed: $MTEST_SEED, else a constant (tests/common/mod.rs). */
time_t mtest_time(time_t *t)
{
	const char *s = getenv("MTEST_SEED");
	time_t v = s ? (time_t)strtol(s, NULL, 10) : (time_t)20261030;

	if (t)
		*t = v;
	return v;
}

/* The key in hex (the MDB_DEBUG version prints it similarly). */
char *mdb_dkey(MDB_val *key, char *buf)
{
	char *p = buf;
	unsigned char *c = key->mv_data;
	size_t i;

	buf[0] = 0;
	for (i = 0; i < key->mv_size && i < 511; i++)
		p += sprintf(p, "%02x", c[i]);
	return buf;
}
