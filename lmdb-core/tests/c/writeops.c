/* Test helper for differential testing of write operations and of the
 * cursor fixups they cause.
 *
 *   writeops <env> <dbname|-> <dbflags> <ncursors> < script
 *
 * (one op per line on stdin).
 * Creates/opens the environment (MDB_NOSYNC, 64 MiB map), begins a write
 * txn, opens (creating) the database with <dbflags> (decimal) and opens
 * <ncursors> cursors on it. Then performs each op and prints one line:
 *
 *   <op> rc=<rc> | c0 rc=<rc> k=<hex> d=<hex> n=<count> | c1 ...
 *
 * where each cN part is an MDB_GET_CURRENT (and, if that succeeded on a
 * DUPSORT database, mdb_cursor_count) on cursor N right after the op.
 * Ops ("<k>"/"<d>" are hex, flags decimal):
 *
 *   p:<k>:<d>:<flags>        mdb_put
 *   cp:<n>:<k>:<d>:<flags>   mdb_cursor_put on cursor n
 *   d:<k>[:<d>]              mdb_del
 *   cd:<n>:<flags>           mdb_cursor_del on cursor n
 *   g:<n>:<opnum>[:<k>[:<d>]] mdb_cursor_get on cursor n (MDB_cursor_op number)
 *
 * Finally commits and prints "commit rc=<rc>".
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lmdb.h"

#define MAXC 8

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
	while (s[0] && s[1] && s[0] != ':' && sscanf(s, "%2x", &b) == 1) {
		out[n++] = (unsigned char)b;
		s += 2;
	}
	return n;
}

/* Splits "a:b:c" in place into at most 6 fields. */
static int fields(char *s, char **f)
{
	int n = 0;
	f[n++] = s;
	while (n < 6 && (s = strchr(s, ':'))) {
		*s++ = 0;
		f[n++] = s;
	}
	return n;
}

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_cursor *cur[MAXC];
	unsigned int dbflags;
	int i, rc, ncur, dupsort;
	static unsigned char kbuf[4096], dbuf[1 << 20];
	static char arg[(1 << 21) + 64];

	if (argc != 5) { fprintf(stderr, "usage\n"); return 2; }
	dbflags = (unsigned int)strtoul(argv[3], NULL, 10);
	ncur = atoi(argv[4]);
	if (ncur < 0 || ncur > MAXC) { fprintf(stderr, "ncursors\n"); return 2; }
	dupsort = (dbflags & MDB_DUPSORT) != 0;
	if ((rc = mdb_env_create(&env)) || (rc = mdb_env_set_maxdbs(env, 8)) ||
		(rc = mdb_env_set_mapsize(env, 64 << 20)) ||
		(rc = mdb_env_open(env, argv[1], MDB_NOSYNC, 0664)) ||
		(rc = mdb_txn_begin(env, NULL, 0, &txn)) ||
		(rc = mdb_dbi_open(txn, strcmp(argv[2], "-") ? argv[2] : NULL,
			dbflags | MDB_CREATE, &dbi))) {
		fprintf(stderr, "setup: %s\n", mdb_strerror(rc));
		return 2;
	}
	for (i = 0; i < ncur; i++) {
		if ((rc = mdb_cursor_open(txn, dbi, &cur[i]))) {
			fprintf(stderr, "cursor: %s\n", mdb_strerror(rc));
			return 2;
		}
	}
	while (fgets(arg, sizeof(arg), stdin)) {
		char *f[6];
		int nf, c;
		MDB_val key = {0, NULL}, data = {0, NULL};
		arg[strcspn(arg, "\r\n")] = 0;
		if (!arg[0])
			continue;
		nf = fields(arg, f);
		if (!strcmp(f[0], "p") && nf == 4) {
			key.mv_data = kbuf; key.mv_size = unhex(f[1], kbuf);
			data.mv_data = dbuf; data.mv_size = unhex(f[2], dbuf);
			rc = mdb_put(txn, dbi, &key, &data, (unsigned)atoi(f[3]));
		} else if (!strcmp(f[0], "cp") && nf == 5) {
			key.mv_data = kbuf; key.mv_size = unhex(f[2], kbuf);
			data.mv_data = dbuf; data.mv_size = unhex(f[3], dbuf);
			rc = mdb_cursor_put(cur[atoi(f[1])], &key, &data, (unsigned)atoi(f[4]));
		} else if (!strcmp(f[0], "d") && (nf == 2 || nf == 3)) {
			key.mv_data = kbuf; key.mv_size = unhex(f[1], kbuf);
			if (nf == 3) {
				data.mv_data = dbuf; data.mv_size = unhex(f[2], dbuf);
			}
			rc = mdb_del(txn, dbi, &key, nf == 3 ? &data : NULL);
		} else if (!strcmp(f[0], "cd") && nf == 3) {
			rc = mdb_cursor_del(cur[atoi(f[1])], (unsigned)atoi(f[2]));
		} else if (!strcmp(f[0], "g") && nf >= 3) {
			if (nf >= 4) { key.mv_data = kbuf; key.mv_size = unhex(f[3], kbuf); }
			if (nf >= 5) { data.mv_data = dbuf; data.mv_size = unhex(f[4], dbuf); }
			rc = mdb_cursor_get(cur[atoi(f[1])], &key, &data, (MDB_cursor_op)atoi(f[2]));
		} else {
			fprintf(stderr, "bad op %s\n", f[0]);
			return 2;
		}
		printf("%s rc=%d", f[0], rc);
		for (c = 0; c < ncur; c++) {
			MDB_val ck = {0, NULL}, cd = {0, NULL};
			size_t n = 0;
			rc = mdb_cursor_get(cur[c], &ck, &cd, MDB_GET_CURRENT);
			printf(" | c%d rc=%d k=", c, rc);
			if (rc == 0) hexout(&ck); else putchar('-');
			printf(" d=");
			if (rc == 0) hexout(&cd); else putchar('-');
			if (rc == 0 && dupsort) {
				rc = mdb_cursor_count(cur[c], &n);
				printf(" n=%d:%zu", rc, rc ? (size_t)0 : n);
			}
		}
		putchar('\n');
		fflush(stdout);
	}
	for (i = 0; i < ncur; i++)
		mdb_cursor_close(cur[i]);
	rc = mdb_txn_commit(txn);
	printf("commit rc=%d\n", rc);
	mdb_env_close(env);
	return 0;
}
