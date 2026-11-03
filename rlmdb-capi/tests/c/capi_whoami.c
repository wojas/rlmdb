/* Which LMDB a program runs on.
 *
 *   capi_whoami <dir>
 *
 * Prints what mdb_version() returns and creates an environment in <dir>
 * with no settings, so that the caller can tell which library ran and
 * which format it gave a new environment. Uses lmdb.h's functions only
 * (C 0.9's or C 1.0's header): the install test links it against a shared
 * C LMDB and runs it with rlmdb's stand-ins for C in front, or links it
 * against an installed librlmdb.
 */
#include <stdio.h>
#include <stdlib.h>
/* mkdir; its Windows form (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"

int main(int argc, char **argv)
{
	MDB_env *env;
	int rc, major, minor, patch;
	char *version;

	if (argc != 2) {
		fprintf(stderr, "usage: capi_whoami <dir>\n");
		return 2;
	}
	version = mdb_version(&major, &minor, &patch);
	printf("%d.%d.%d %s\n", major, minor, patch, version);
	mkdir(argv[1], 0775);
	if ((rc = mdb_env_create(&env)) != 0 || (rc = mdb_env_open(env, argv[1], 0, 0664)) != 0) {
		fprintf(stderr, "capi_whoami: %s\n", mdb_strerror(rc));
		return 1;
	}
	mdb_env_close(env);
	return 0;
}
