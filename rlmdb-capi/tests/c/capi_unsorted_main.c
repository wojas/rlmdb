/* A main DB out of order: what a nested transaction's abort does to the
 * handles of its parent's databases, and where a new database's record
 * goes.
 *
 *   capi_unsorted_main <scratch-dir>
 *
 * Built against C LMDB 0.9 and 1.0 (the oracles) and against the Rust
 * libraries with -DLMDB_RUST, each with its own lmdb.h: a 0.9 header makes
 * 0.9 environments, 1.0's makes 1.0 environments (whose database names are
 * stored with their NUL).
 *
 * lmdb.h requires a database's comparator to stay the same. This program
 * changes the main DB's within a write transaction, a misuse C does not
 * detect (as it cannot detect a comparator changed between opens, or a
 * damaged file): the names "dups" (one item), "fixed", "ints" and "plain"
 * are stored in LMDB's default order, then the key 89 8a 8b 8c in an order
 * that puts bytes from 0x80 first, which puts it at index 0. In the
 * default order again the main DB is out of order: a search for "dups"
 * turns left at "fixed", then at 89 8a 8b 8c, and misses it.
 *
 * Then, in each of three environments:
 *
 *   plain    a nested transaction begins and aborts; the parent commits.
 *   reuse    the same, then "xa" is created before the commit.
 *   nochild  "xa" is created and the transaction commits, without a
 *            nested one.
 *
 * C closes only the handles of databases a nested transaction created
 * (mdb_dbis_update): "dups" keeps its handle, its record is written at
 * commit and "xa" gets a slot of its own. "xa"'s record goes where the
 * cursor of mdb_dbi_open's search is left: the put's mdb_cursor_set starts
 * on that leaf and puts a name before its first key at index 0.
 *
 * A fourth environment, "sorted", has a main DB in order and of several
 * pages: 600 databases created in a scrambled order, half in a second
 * transaction, where a search would put them, as C does.
 *
 * The program prints a trace ("T ..." lines) of the return codes, the
 * handles and the committed main DB's records. The test driver requires
 * the traces of C and Rust (of the same format) to be identical, their
 * data files to be the same byte for byte, and both to pass the C page
 * audit.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* mkdir; its Windows form (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"

#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

#define E(expr) do { int rc_ = (expr); if (rc_) { \
	fprintf(stderr, "%s:%d: %s: %s\n", __FILE__, __LINE__, #expr, mdb_strerror(rc_)); \
	exit(2); } } while (0)

/* LMDB 1.0 stores a database's name with its NUL. */
#if MDB_VERSION_MAJOR >= 1
#define NAME_NUL 1
#else
#define NAME_NUL 0
#endif

/* LMDB's default key order (mdb_cmp_memn). */
static int memn(const MDB_val *a, const MDB_val *b)
{
	size_t n = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;
	int r = memcmp(a->mv_data, b->mv_data, n);
	if (r)
		return r;
	return a->mv_size < b->mv_size ? -1 : a->mv_size > b->mv_size;
}

/* The default order with the top bit of every byte flipped: bytes from
 * 0x80 sort before the others. */
static int hibit_first(const MDB_val *a, const MDB_val *b)
{
	const unsigned char *x = a->mv_data, *y = b->mv_data;
	size_t i, n = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;
	for (i = 0; i < n; i++) {
		int d = (x[i] ^ 0x80) - (y[i] ^ 0x80);
		if (d)
			return d;
	}
	return a->mv_size < b->mv_size ? -1 : a->mv_size > b->mv_size;
}

static void hex(char *out, const MDB_val *v)
{
	size_t i;
	for (i = 0; i < v->mv_size && i < 32; i++)
		sprintf(out + 2 * i, "%02x", ((unsigned char *)v->mv_data)[i]);
	out[2 * i] = '\0';
}

/* The committed main DB's records in page order (with `records`; a named
 * database's record shows its entries and root), its depth and page
 * counts. */
static void list_main(MDB_env *env, MDB_dbi main_dbi, const char *what, int records)
{
	MDB_txn *txn;
	MDB_cursor *c;
	MDB_val k, v;
	MDB_envinfo ei;
	MDB_stat st;
	char buf[80];
	int rc;

	E(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	E(mdb_cursor_open(txn, main_dbi, &c));
	while ((rc = mdb_cursor_get(c, &k, &v, MDB_NEXT)) == 0) {
		if (!records)
			continue;
		hex(buf, &k);
		if (v.mv_size == 48) {
			unsigned long long entries, root;
			memcpy(&entries, (char *)v.mv_data + 32, 8);
			memcpy(&root, (char *)v.mv_data + 40, 8);
			T("%s: %s: DB entries %llu root %lld", what, buf, entries, (long long)root);
		} else {
			T("%s: %s: %u bytes", what, buf, (unsigned)v.mv_size);
		}
	}
	T("%s: end of the main DB: %d", what, rc);
	mdb_cursor_close(c);
	E(mdb_env_info(env, &ei));
	E(mdb_stat(txn, main_dbi, &st));
	T("%s: last page %u, main DB: depth %u, branch pages %u, leaf pages %u, entries %u",
		what, (unsigned)ei.me_last_pgno, st.ms_depth, (unsigned)st.ms_branch_pages,
		(unsigned)st.ms_leaf_pages, (unsigned)st.ms_entries);
	mdb_txn_abort(txn);
}

static void run(const char *scratch, const char *what, int child, int reuse)
{
	MDB_env *env;
	MDB_txn *txn, *nested;
	MDB_dbi main_dbi, dups, dbi, xa;
	MDB_val k, v;
	char dir[1024], big[10];
	int rc;

	snprintf(dir, sizeof dir, "%s/%s", scratch, what);
	mkdir(dir, 0755);
	memset(big, 'v', sizeof big);
	E(mdb_env_create(&env));
	E(mdb_env_set_maxdbs(env, 8));
	E(mdb_env_open(env, dir, MDB_NOSYNC, 0644));
	E(mdb_txn_begin(env, NULL, 0, &txn));
	E(mdb_dbi_open(txn, "dups", MDB_CREATE, &dups));
	k.mv_data = "k"; k.mv_size = 1;
	v.mv_data = "v"; v.mv_size = 1;
	E(mdb_put(txn, dups, &k, &v, 0));
	E(mdb_dbi_open(txn, "fixed", MDB_CREATE, &dbi));
	E(mdb_dbi_open(txn, "ints", MDB_CREATE, &dbi));
	E(mdb_dbi_open(txn, "plain", MDB_CREATE, &dbi));
	/* (Before the comparators are set: C's mdb_dbi_open of the main DB
	 * puts the default one back.) */
	E(mdb_dbi_open(txn, NULL, 0, &main_dbi));

	/* The misuse: another key order for one put. */
	E(mdb_set_compare(txn, main_dbi, hibit_first));
	k.mv_data = "\x89\x8a\x8b\x8c"; k.mv_size = 4;
	v.mv_data = big; v.mv_size = sizeof big;
	E(mdb_put(txn, main_dbi, &k, &v, MDB_NOOVERWRITE));
	E(mdb_set_compare(txn, main_dbi, memn));
	k.mv_data = "dups"; k.mv_size = 4 + NAME_NUL;
	T("%s: search for dups' record: %d", what, mdb_get(txn, main_dbi, &k, &v));

	if (child) {
		E(mdb_txn_begin(env, txn, 0, &nested));
		mdb_txn_abort(nested);
	}
	if (reuse) {
		rc = mdb_dbi_open(txn, "xa", MDB_CREATE, &xa);
		T("%s: create xa: %d, dups dbi %u, xa dbi %u", what, rc, dups, xa);
	}
	k.mv_data = "k"; k.mv_size = 1;
	rc = mdb_get(txn, dups, &k, &v);
	T("%s: get k from dups: %d (%u bytes)", what, rc, rc ? 0 : (unsigned)v.mv_size);
	T("%s: commit: %d", what, mdb_txn_commit(txn));
	list_main(env, main_dbi, what, 1);
	mdb_env_close(env);
}

/* The "sorted" environment: databases "database-0000" to "database-0599"
 * created in a scrambled order, the even ones in a first transaction, the
 * odd ones in a second, with an item in every 50th. Every key has an even
 * length (in 0.9 the names end in "x"; 1.0 stores them with their NUL), so
 * that no node has a padding byte: C leaves it as a reused page buffer
 * had it, and the test driver compares the files byte for byte (except
 * the unused space between mp_lower and mp_upper, for the same reason). */
static void run_sorted(const char *scratch)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi, main_dbi;
	MDB_val k, v;
	char dir[1024], name[32];
	unsigned int i, n, pass;

	snprintf(dir, sizeof dir, "%s/sorted", scratch);
	mkdir(dir, 0755);
	E(mdb_env_create(&env));
	E(mdb_env_set_maxdbs(env, 600));
	E(mdb_env_set_mapsize(env, 8 << 20));
	E(mdb_env_open(env, dir, MDB_NOSYNC, 0644));
	for (pass = 0; pass < 2; pass++) {
		E(mdb_txn_begin(env, NULL, 0, &txn));
		for (i = 0; i < 600; i++) {
			n = i * 353 % 600;
			if (n % 2 != pass)
				continue;
			sprintf(name, "database-%04u%s", n, NAME_NUL ? "" : "x");
			E(mdb_dbi_open(txn, name, MDB_CREATE, &dbi));
			if (n % 50 == 1) {
				k.mv_data = "kk"; k.mv_size = 2;
				v.mv_data = "vv"; v.mv_size = 2;
				E(mdb_put(txn, dbi, &k, &v, 0));
			}
		}
		E(mdb_txn_commit(txn));
	}
	E(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	E(mdb_dbi_open(txn, NULL, 0, &main_dbi));
	mdb_txn_abort(txn);
	list_main(env, main_dbi, "sorted", 0);
	mdb_env_close(env);
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: capi_unsorted_main <scratch-dir>\n");
		return 2;
	}
	printf("library: %s\n", mdb_version(NULL, NULL, NULL));
	run(argv[1], "plain", 1, 0);
	run(argv[1], "reuse", 1, 1);
	run(argv[1], "nochild", 0, 1);
	run_sorted(argv[1]);
	return 0;
}
