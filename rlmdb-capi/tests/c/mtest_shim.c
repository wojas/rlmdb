/* See mtest_shim.h. */
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

/* The header is force-included before <stdlib.h>, whose declarations
 * of rand() and srand() the macros renamed: the libc's are these. */
#undef srand
#undef rand
void srand(unsigned seed);
int rand(void);

static int seeded;

/* srand(): the seeding call of this libc (OpenBSD's srand() ignores its
 * argument). */
void mtest_srand(unsigned seed)
{
	seeded = 1;
#ifdef __OpenBSD__
	srand_deterministic(seed);
#else
	srand(seed);
#endif
}

/* rand(), seeded with the test seed first if the program did not seed
 * it (mtest4), so that every libc gives the same sequence twice. */
int mtest_rand(void)
{
	if (!seeded)
		mtest_srand((unsigned)mtest_time(NULL));
	return rand();
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
