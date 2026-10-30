/* Test helper: keeps an LMDB environment open (with the C library) so the
 * reader table can be inspected while it runs.
 *
 *   holder [-n] [-r] <path>
 *     -n  MDB_NOSUBDIR
 *     -r  begin a read-only transaction and keep it open
 *
 * Prints "READY <pid>\n" once set up, then waits until stdin is closed (or
 * the process is killed).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "lmdb.h"

#define CHECK(rc, what) do { if (rc) { fprintf(stderr, "%s: %s\n", what, mdb_strerror(rc)); exit(2); } } while (0)

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn = NULL;
	unsigned int flags = 0;
	int rdtxn = 0, i, rc;
	char line[64];

	if (argc < 2) { fprintf(stderr, "usage: holder [-n] [-r] path\n"); return 2; }
	for (i = 1; i < argc - 1; i++) {
		if (!strcmp(argv[i], "-n")) flags |= MDB_NOSUBDIR;
		else if (!strcmp(argv[i], "-r")) rdtxn = 1;
	}
	rc = mdb_env_create(&env); CHECK(rc, "env_create");
	rc = mdb_env_open(env, argv[argc - 1], flags | MDB_RDONLY, 0664); CHECK(rc, "env_open");
	if (rdtxn) {
		rc = mdb_txn_begin(env, NULL, MDB_RDONLY, &txn); CHECK(rc, "txn_begin");
	}
	printf("READY %d\n", (int)getpid());
	fflush(stdout);
	while (fgets(line, sizeof(line), stdin))
		;
	if (txn)
		mdb_txn_abort(txn);
	mdb_env_close(env);
	return 0;
}
