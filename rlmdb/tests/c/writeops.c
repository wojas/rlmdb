/* Test helper for differential testing of write operations and of the
 * cursor fixups they cause.
 *
 *   writeops <env> <dbname|-> <dbflags> <ncursors> [<mapsize> [<pagesize>]] < script
 *
 * (one op per line on stdin).
 * Creates/opens the environment (MDB_NOSYNC, 64 MiB map unless <mapsize>
 * is given; <pagesize>, if not 0, with mdb_env_set_pagesize, which only
 * LMDB 1.0 has: built against 0.9 a page size is an error), begins a write txn, opens (creating) the database with
 * <dbflags> (decimal) and opens <ncursors> cursors on it. Then performs
 * each op and prints one line:
 *
 *   <op> rc=<rc> | c0 rc=<rc> k=<hex> d=<hex> n=<count> | c1 ...
 *
 * where each cN part is an MDB_GET_CURRENT (and, if that succeeded on a
 * DUPSORT database, mdb_cursor_count) on cursor N right after the op.
 * Cursors are numbered across nesting levels: in a transaction at depth d,
 * 0..ncursors-1 are its own, the next <ncursors> its parent's (used through
 * the child, as C allows), and so on up to the top-level transaction's.
 * Ops ("<k>"/"<d>" are hex, flags decimal):
 *
 *   p:<k>:<d>:<flags>        mdb_put
 *   cp:<n>:<k>:<d>:<flags>   mdb_cursor_put on cursor n
 *   d:<k>[:<d>]              mdb_del
 *   cd:<n>:<flags>           mdb_cursor_del on cursor n
 *   cr:<n>:<k>:<d>           mdb_cursor_del on cursor n, then (if that
 *                            succeeded) mdb_cursor_put of k/d with flags 0
 *                            on it: what rlmdb's MDB_CURRENT does where C's
 *                            in-place replacement goes wrong (the Rust side
 *                            of the harness runs it as one MDB_CURRENT put)
 *   g:<n>:<opnum>[:<k>[:<d>]] mdb_cursor_get on cursor n (MDB_cursor_op number)
 *   nb                       begin a nested txn of the current one and open
 *                            <ncursors> cursors in it (they become current)
 *   nc / na                  close the current txn's cursors and commit /
 *                            abort it (nested txns only); its parent's
 *                            cursors become current again
 *   dr:<del>                 mdb_drop(txn, dbi, del); with del=1 (top-level
 *                            txn only) the cursors are closed first, and the
 *                            DB is created again and the cursors reopened
 *   b:<n>:<seed>:<len>       n mdb_puts of 8-byte keys and <len>-byte values
 *                            from a SplitMix64 stream (see bulk_put); rc is
 *                            the first failure
 *
 * Finally commits and prints "commit rc=<rc>".
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include "lmdb.h"
#ifdef _WIN32
/* Binary stdio and getpid (tests/compat, on the include path there). */
#include "posix_compat.h"
#endif

#define MAXC 8
#define MAXDEPTH 8

static uint64_t splitmix64(uint64_t *state)
{
	uint64_t z = (*state += 0x9e3779b97f4a7c15ull);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
	return z ^ (z >> 31);
}

/* Bulk puts: key = the next SplitMix64 word (8 bytes, little-endian),
 * value = <len> bytes of further words. */
static int bulk_put(MDB_txn *txn, MDB_dbi dbi, unsigned long n, uint64_t seed, size_t len)
{
	static unsigned char vbuf[1 << 20];
	unsigned char kbuf[8];
	uint64_t st = seed, x = 0;
	unsigned long i;
	size_t j;
	int rc;
	if (len > sizeof(vbuf))
		return EINVAL;
	for (i = 0; i < n; i++) {
		MDB_val key, data;
		x = splitmix64(&st);
		for (j = 0; j < 8; j++)
			kbuf[j] = (unsigned char)(x >> (8 * j));
		for (j = 0; j < len; j++) {
			if (j % 8 == 0)
				x = splitmix64(&st);
			vbuf[j] = (unsigned char)(x >> (8 * (j % 8)));
		}
		key.mv_data = kbuf; key.mv_size = 8;
		data.mv_data = vbuf; data.mv_size = len;
		if ((rc = mdb_put(txn, dbi, &key, &data, 0)))
			return rc;
	}
	return 0;
}

/* mdb_env_set_pagesize where the library has it (LMDB 1.0). */
static int set_pagesize(MDB_env *env, int size)
{
	if (!size)
		return 0;
#if MDB_VERSION_MAJOR >= 1
	return mdb_env_set_pagesize(env, size);
#else
	(void)env;
	return EINVAL;
#endif
}

static int open_cursors(MDB_txn *txn, MDB_dbi dbi, MDB_cursor **cur, int ncur)
{
	int i, rc;
	for (i = 0; i < ncur; i++) {
		if ((rc = mdb_cursor_open(txn, dbi, &cur[i])))
			return rc;
	}
	return 0;
}

static void close_cursors(MDB_cursor **cur, int ncur)
{
	int i;
	for (i = 0; i < ncur; i++)
		mdb_cursor_close(cur[i]);
}

static void hexout(const MDB_val *v)
{
	size_t i;
	if (!v->mv_data) { putchar('-'); return; }
	for (i = 0; i < v->mv_size; i++)
		printf("%02x", ((unsigned char *)v->mv_data)[i]);
}

static int hexval(char c)
{
	if (c >= '0' && c <= '9')
		return c - '0';
	if (c >= 'a' && c <= 'f')
		return c - 'a' + 10;
	if (c >= 'A' && c <= 'F')
		return c - 'A' + 10;
	return -1;
}

/* Decodes hex digit pairs up to the first non-hex character. (Not with
 * sscanf: glibc's scans the rest of the string on every call, which makes
 * decoding a value of a few hundred KiB take minutes.) */
static size_t unhex(const char *s, unsigned char *out)
{
	size_t n = 0;
	int hi, lo;
	while ((hi = hexval(s[0])) >= 0 && (lo = hexval(s[1])) >= 0) {
		out[n++] = (unsigned char)(hi << 4 | lo);
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
	MDB_txn *txns[MAXDEPTH], *txn;
	MDB_dbi dbi;
	MDB_cursor *curs[MAXDEPTH][MAXC], **cur;
	unsigned int dbflags;
	const char *dbname;
	size_t mapsize = 64 << 20;
	int pagesize = 0;
	int rc, ncur, dupsort, depth = 0;
	/* Keys up to LMDB 1.0's largest (32,698 bytes with 64 KiB pages). */
	static unsigned char kbuf[1 << 16], dbuf[1 << 20];
	static char arg[(1 << 21) + 64];

	if (argc < 5 || argc > 7) { fprintf(stderr, "usage\n"); return 2; }
	dbname = strcmp(argv[2], "-") ? argv[2] : NULL;
	dbflags = (unsigned int)strtoul(argv[3], NULL, 10);
	ncur = atoi(argv[4]);
	if (argc >= 6)
		mapsize = (size_t)strtoull(argv[5], NULL, 10);
	if (argc == 7)
		pagesize = (int)strtol(argv[6], NULL, 10);
	if (ncur < 0 || ncur > MAXC) { fprintf(stderr, "ncursors\n"); return 2; }
	dupsort = (dbflags & MDB_DUPSORT) != 0;
	if ((rc = mdb_env_create(&env)) || (rc = mdb_env_set_maxdbs(env, 8)) ||
			(rc = mdb_env_set_mapsize(env, mapsize)) ||
		(rc = set_pagesize(env, pagesize)) ||
		(rc = mdb_env_open(env, argv[1], MDB_NOSYNC, 0664)) ||
		(rc = mdb_txn_begin(env, NULL, 0, &txns[0])) ||
		(rc = mdb_dbi_open(txns[0], dbname, dbflags | MDB_CREATE, &dbi))) {
		fprintf(stderr, "setup: %s\n", mdb_strerror(rc));
		return 2;
	}
	if ((rc = open_cursors(txns[0], dbi, curs[0], ncur))) {
		fprintf(stderr, "cursor: %s\n", mdb_strerror(rc));
		return 2;
	}
	txn = txns[0];
	cur = curs[0];
#define CUR(n) curs[depth - (n) / ncur][(n) % ncur]
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
			rc = mdb_cursor_put(CUR(atoi(f[1])), &key, &data, (unsigned)atoi(f[4]));
		} else if (!strcmp(f[0], "d") && (nf == 2 || nf == 3)) {
			key.mv_data = kbuf; key.mv_size = unhex(f[1], kbuf);
			if (nf == 3) {
				data.mv_data = dbuf; data.mv_size = unhex(f[2], dbuf);
			}
			rc = mdb_del(txn, dbi, &key, nf == 3 ? &data : NULL);
		} else if (!strcmp(f[0], "cd") && nf == 3) {
			rc = mdb_cursor_del(CUR(atoi(f[1])), (unsigned)atoi(f[2]));
		} else if (!strcmp(f[0], "cr") && nf == 4) {
			key.mv_data = kbuf; key.mv_size = unhex(f[2], kbuf);
			data.mv_data = dbuf; data.mv_size = unhex(f[3], dbuf);
			rc = mdb_cursor_del(CUR(atoi(f[1])), 0);
			if (!rc)
				rc = mdb_cursor_put(CUR(atoi(f[1])), &key, &data, 0);
		} else if (!strcmp(f[0], "g") && nf >= 3) {
			if (nf >= 4) { key.mv_data = kbuf; key.mv_size = unhex(f[3], kbuf); }
			if (nf >= 5) { data.mv_data = dbuf; data.mv_size = unhex(f[4], dbuf); }
			rc = mdb_cursor_get(CUR(atoi(f[1])), &key, &data, (MDB_cursor_op)atoi(f[2]));
		} else if (!strcmp(f[0], "nb") && nf == 1) {
			if (depth + 1 >= MAXDEPTH) { fprintf(stderr, "too deep\n"); return 2; }
			rc = mdb_txn_begin(env, txn, 0, &txns[depth + 1]);
			if (!rc) {
				depth++;
				txn = txns[depth];
				cur = curs[depth];
				if ((rc = open_cursors(txn, dbi, cur, ncur))) {
					fprintf(stderr, "cursor: %s\n", mdb_strerror(rc));
					return 2;
				}
			}
		} else if ((!strcmp(f[0], "nc") || !strcmp(f[0], "na")) && nf == 1) {
			if (!depth) { fprintf(stderr, "not nested\n"); return 2; }
			close_cursors(cur, ncur);
			if (f[0][1] == 'c') {
				rc = mdb_txn_commit(txn);
			} else {
				mdb_txn_abort(txn);
				rc = 0;
			}
			depth--;
			txn = txns[depth];
			cur = curs[depth];
		} else if (!strcmp(f[0], "dr") && nf == 2) {
			int del = atoi(f[1]);
			if (del) {
				if (depth) { fprintf(stderr, "dr:1 nested\n"); return 2; }
				close_cursors(cur, ncur);
			}
			rc = mdb_drop(txn, dbi, del);
			if (del) {
				int rc2 = mdb_dbi_open(txn, dbname, dbflags | MDB_CREATE, &dbi);
				if (rc2 || (rc2 = open_cursors(txn, dbi, cur, ncur))) {
					fprintf(stderr, "reopen: %s\n", mdb_strerror(rc2));
					return 2;
				}
			}
		} else if (!strcmp(f[0], "b") && nf == 4) {
			rc = bulk_put(txn, dbi, strtoul(f[1], NULL, 10),
				(uint64_t)strtoull(f[2], NULL, 10), (size_t)strtoul(f[3], NULL, 10));
		} else {
			fprintf(stderr, "bad op %s\n", f[0]);
			return 2;
		}
		printf("%s rc=%d", f[0], rc);
		for (c = 0; c < ncur * (depth + 1); c++) {
			MDB_val ck = {0, NULL}, cd = {0, NULL};
			size_t n = 0;
			rc = mdb_cursor_get(CUR(c), &ck, &cd, MDB_GET_CURRENT);
			printf(" | c%d rc=%d k=", c, rc);
			if (rc == 0) hexout(&ck); else putchar('-');
			printf(" d=");
			if (rc == 0) hexout(&cd); else putchar('-');
			if (rc == 0 && dupsort) {
				rc = mdb_cursor_count(CUR(c), &n);
				printf(" n=%d:%zu", rc, rc ? (size_t)0 : n);
			}
		}
		putchar('\n');
		fflush(stdout);
	}
	if (depth) { fprintf(stderr, "nested txn left open\n"); return 2; }
	close_cursors(cur, ncur);
	rc = mdb_txn_commit(txn);
	printf("commit rc=%d\n", rc);
	mdb_env_close(env);
	return 0;
}
