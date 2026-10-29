/* Test helper for differential testing of cursor semantics.
 *
 *   cursorops [-n] <env> <dbname|-> <op>...
 *
 * Opens the environment read-only, a read txn and a cursor on the database
 * ("-" = main DB), then performs each op with mdb_cursor_get and prints one
 * line per op:
 *
 *   <op> rc=<rc> d=<hex|-> | cur rc=<rc> k=<hex> d=<hex>
 *
 * where the first part is the op's result (data printed only on success; "-"
 * if the op left data untouched) and the second part is an MDB_GET_CURRENT
 * done right after it, showing where the cursor ended up. An op is
 * "<name>[:<hexkey>[:<hexdata>]]"; "count" calls mdb_cursor_count instead.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lmdb.h"

static const char *names[] = {
	"first", "first_dup", "get_both", "get_both_range", "get_current",
	"get_multiple", "last", "last_dup", "next", "next_dup", "next_multiple",
	"next_nodup", "prev", "prev_dup", "prev_nodup", "set", "set_key",
	"set_range", "prev_multiple", NULL
};

static void hexout(const MDB_val *v)
{
	size_t i;
	if (!v->mv_data) { putchar('-'); return; }
	for (i = 0; i < v->mv_size; i++)
		printf("%02x", ((unsigned char *)v->mv_data)[i]);
}

static size_t unhex(const char *s, unsigned char *out)
{
	size_t n = 0;
	unsigned int b;
	while (s[0] && s[1] && sscanf(s, "%2x", &b) == 1) {
		out[n++] = (unsigned char)b;
		s += 2;
	}
	return n;
}

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_cursor *cur;
	unsigned int flags = MDB_RDONLY;
	int i, rc, argi = 1;
	static unsigned char kbuf[4096], dbuf[65536];

	if (argc > 1 && !strcmp(argv[1], "-n")) { flags |= MDB_NOSUBDIR; argi++; }
	if (argc - argi < 2) { fprintf(stderr, "usage\n"); return 2; }
	if ((rc = mdb_env_create(&env)) || (rc = mdb_env_set_maxdbs(env, 8)) ||
		(rc = mdb_env_open(env, argv[argi], flags, 0664)) ||
		(rc = mdb_txn_begin(env, NULL, MDB_RDONLY, &txn)) ||
		(rc = mdb_dbi_open(txn, strcmp(argv[argi+1], "-") ? argv[argi+1] : NULL, 0, &dbi)) ||
		(rc = mdb_cursor_open(txn, dbi, &cur))) {
		fprintf(stderr, "setup: %s\n", mdb_strerror(rc));
		return 2;
	}
	for (i = argi + 2; i < argc; i++) {
		char opname[64], *p;
		MDB_val key = {0, NULL}, data = {0, NULL}, ck, cd;
		int op;
		strncpy(opname, argv[i], sizeof(opname) - 1);
		opname[sizeof(opname) - 1] = 0;
		if ((p = strchr(opname, ':'))) *p = 0;
		p = strchr(argv[i], ':');
		if (p) {
			char *q = strchr(p + 1, ':');
			key.mv_data = kbuf;
			key.mv_size = unhex(p + 1, kbuf);
			if (q) {
				data.mv_data = dbuf;
				data.mv_size = unhex(q + 1, dbuf);
			}
		}
		if (!strcmp(opname, "count")) {
			size_t n = 0;
			rc = mdb_cursor_count(cur, &n);
			printf("count rc=%d n=%zu", rc, rc ? (size_t)0 : n);
		} else {
			for (op = 0; names[op] && strcmp(names[op], opname); op++) ;
			if (!names[op]) { fprintf(stderr, "bad op %s\n", opname); return 2; }
			/* Inputs are only read; outputs replace the vals. Detect
			 * "untouched" data by passing a NULL mv_data where the op
			 * has no data input. */
			rc = mdb_cursor_get(cur, &key, &data, (MDB_cursor_op)op);
			printf("%s rc=%d d=", opname, rc);
			if (rc == 0) {
				/* For ops with a data input, an untouched value is
				 * indistinguishable; they always return data anyway. */
				hexout(&data);
			} else {
				putchar('-');
			}
		}
		ck.mv_size = cd.mv_size = 0;
		ck.mv_data = cd.mv_data = NULL;
		rc = mdb_cursor_get(cur, &ck, &cd, MDB_GET_CURRENT);
		printf(" | cur rc=%d k=", rc);
		if (rc == 0) hexout(&ck); else putchar('-');
		printf(" d=");
		if (rc == 0) hexout(&cd); else putchar('-');
		putchar('\n');
		/* Flush per line: C may hit an assertion on odd sequences. */
		fflush(stdout);
	}
	mdb_cursor_close(cur);
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return 0;
}
