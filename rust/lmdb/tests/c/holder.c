/* Test helper: opens an LMDB environment with the C library, optionally
 * starts a read transaction, reports its state on stdout and waits for
 * commands on stdin.
 *
 *   holder [-n] [-r] [-w] <path>
 *     -n  MDB_NOSUBDIR
 *     -r  begin a read-only transaction and keep it open
 *
 * Output: "READY pid=<pid> txnid=<txnid|->\n". Then reads lines:
 *   "quit"  -> abort the txn, close the env, exit 0
 *   "renew" -> reset + renew the read txn, print "TXN <txnid>"
 * EOF on stdin behaves like "quit".
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
	char line[256];

	for (i = 1; i < argc - 1; i++) {
		if (!strcmp(argv[i], "-n")) flags |= MDB_NOSUBDIR;
		else if (!strcmp(argv[i], "-r")) rdtxn = 1;
	}
	if (argc < 2) { fprintf(stderr, "usage\n"); return 2; }

	rc = mdb_env_create(&env); CHECK(rc, "env_create");
	rc = mdb_env_set_maxdbs(env, 8); CHECK(rc, "set_maxdbs");
	rc = mdb_env_set_mapsize(env, 64UL << 20); CHECK(rc, "set_mapsize");
	rc = mdb_env_open(env, argv[argc - 1], flags, 0664); CHECK(rc, "env_open");
	if (rdtxn) {
		rc = mdb_txn_begin(env, NULL, MDB_RDONLY, &txn); CHECK(rc, "txn_begin");
		printf("READY pid=%d txnid=%zu\n", (int)getpid(), mdb_txn_id(txn));
	} else {
		printf("READY pid=%d txnid=-\n", (int)getpid());
	}
	fflush(stdout);
	while (fgets(line, sizeof(line), stdin)) {
		if (!strncmp(line, "quit", 4))
			break;
		if (!strncmp(line, "renew", 5) && txn) {
			mdb_txn_reset(txn);
			rc = mdb_txn_renew(txn); CHECK(rc, "txn_renew");
			printf("TXN %zu\n", mdb_txn_id(txn));
			fflush(stdout);
		}
	}
	if (txn)
		mdb_txn_abort(txn);
	mdb_env_close(env);
	return 0;
}
