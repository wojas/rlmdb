/* Checks the limits the Rust library has where C LMDB has none (there is
 * no C build: C accepts what is refused here; KNOWN_C_BUGS.md):
 *
 *   capi_limits <env-dir>
 *
 * - mdb_env_set_maxdbs beyond MAX_DBS_LIMIT (1 << 20) is EINVAL (row 54).
 * - mdb_env_set_maxreaders beyond MAX_READERS_LIMIT (1 << 20) is EINVAL
 *   (row 60); 0 is EINVAL as in C.
 *
 * Prints "capi_limits: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include "lmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	unsigned int readers;

	if (argc != 2) {
		fprintf(stderr, "usage: capi_limits <env-dir>\n");
		return 2;
	}
	CHECK(mdb_env_create(&env) == 0);
	CHECK(mdb_env_set_maxdbs(env, (1u << 20) + 1) == EINVAL);
	CHECK(mdb_env_set_maxdbs(env, (MDB_dbi)-1) == EINVAL);
	CHECK(mdb_env_set_maxdbs(env, 1u << 20) == 0);
	CHECK(mdb_env_set_maxdbs(env, 100000) == 0);
	CHECK(mdb_env_set_maxreaders(env, (1u << 20) + 1) == EINVAL);
	CHECK(mdb_env_set_maxreaders(env, (unsigned int)-1) == EINVAL);
	CHECK(mdb_env_set_maxreaders(env, 0) == EINVAL); /* as in C */
	CHECK(mdb_env_set_maxreaders(env, 1u << 20) == 0);
	CHECK(mdb_env_set_maxreaders(env, 100000) == 0);
	CHECK(mdb_env_open(env, argv[1], 0, 0664) == 0);
	CHECK(mdb_env_set_maxdbs(env, 1) == EINVAL); /* open: as in C */
	CHECK(mdb_env_set_maxreaders(env, 1) == EINVAL); /* open: as in C */
	CHECK(mdb_env_get_maxreaders(env, &readers) == 0 && readers == 100000);
	CHECK(mdb_txn_begin(env, NULL, 0, &txn) == 0);
	CHECK(mdb_dbi_open(txn, "one", MDB_CREATE, &dbi) == 0);
	CHECK(mdb_txn_commit(txn) == 0);
	mdb_env_close(env);
	printf("capi_limits: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
