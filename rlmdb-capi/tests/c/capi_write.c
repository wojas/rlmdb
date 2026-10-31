/* Exercises the write side of the LMDB C API and its error codes.
 *
 *   capi_write <scratch-dir>
 *
 * Companion of capi_read.c, built the same way: against the C library (the
 * oracle: every expectation below must hold for C LMDB 0.9.36) and against
 * the Rust library with -DLMDB_RUST. GAP() marks C behaviour the Rust
 * implementation does not have yet; DEVIATION() marks a documented,
 * deliberate difference (KNOWN_C_BUGS.md): the Rust build checks the
 * Rust behaviour, the C build skips the call (C would corrupt something).
 *
 * Besides the checks, the program prints a trace ("T ..." lines) of what
 * it observes: statistics, counts, keys and values. The test driver
 * requires the C and Rust traces to be identical, which catches
 * differences no check spells out (page counts, tree depths, ...).
 *
 * Prints "capi_write: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
/* pthreads, mkdir, unistd.h, dlopen; their Windows forms (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"
#include "cmpfuncs.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

#define RC(expr, expected) do { int rc_ = (expr), ex_ = (expected); checks++; \
	if (rc_ != ex_) { failures++; \
		fprintf(stderr, "%s:%d: %s returned %d (%s), expected %d (%s)\n", \
			__FILE__, __LINE__, #expr, rc_, mdb_strerror(rc_), ex_, mdb_strerror(ex_)); \
	} } while (0)

#define OK(expr) RC(expr, 0)

/* A documented difference (README.md): C returns c_rc, Rust rust_rc. */
#ifdef LMDB_RUST
#define DIFFERS(expr, c_rc, rust_rc) RC(expr, rust_rc)
#else
#define DIFFERS(expr, c_rc, rust_rc) RC(expr, c_rc)
#endif

#ifdef LMDB_RUST
#define GAP(name, cond) do { if (cond) { failures++; \
	fprintf(stderr, "%s:%d: known gap \"%s\" now behaves like C: " \
		"update the test and rlmdb-capi/README.md\n", __FILE__, __LINE__, name); \
	} else printf("known gap: %s\n", name); } while (0)
#define DEVIATION(stmt) do { stmt; } while (0)
#else
#define GAP(name, cond) CHECK(cond)
#define DEVIATION(stmt) do { checks++; } while (0)
#endif

/* The two core databases (mdb.c). */
#define FREE_DBI 0
#define MAIN_DBI 1

/* A trace line, compared between the C and Rust builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

static char scratch[1024];

static MDB_val sv(const char *s)
{
	MDB_val v;
	v.mv_size = strlen(s);
	v.mv_data = (void *)s;
	return v;
}

static int veq(MDB_val v, const char *s)
{
	return v.mv_size == strlen(s) && !memcmp(v.mv_data, s, v.mv_size);
}

static MDB_val bv(void *p, size_t n)
{
	MDB_val v;
	v.mv_size = n;
	v.mv_data = p;
	return v;
}

/* A fresh environment in <scratch>/<name> (MDB_NOTLS: the tests hold
 * several read transactions in one thread). */
static MDB_env *new_env(const char *name, size_t mapsize, MDB_dbi maxdbs, unsigned flags)
{
	MDB_env *env;
	char path[1200];

	snprintf(path, sizeof(path), "%s/%s", scratch, name);
	mkdir(path, 0775);
	OK(mdb_env_create(&env));
	OK(mdb_env_set_mapsize(env, mapsize));
	OK(mdb_env_set_maxdbs(env, maxdbs));
	OK(mdb_env_open(env, path, flags | MDB_NOTLS, 0664));
	return env;
}

static void trace_stat(const char *what, MDB_txn *txn, MDB_dbi dbi)
{
	MDB_stat st;
	OK(mdb_stat(txn, dbi, &st));
	T("%s: depth %u branch %zu leaf %zu overflow %zu entries %zu", what,
		st.ms_depth, st.ms_branch_pages, st.ms_leaf_pages,
		st.ms_overflow_pages, st.ms_entries);
}

static size_t entries(MDB_txn *txn, MDB_dbi dbi)
{
	MDB_stat st;
	st.ms_entries = (size_t)-1;
	OK(mdb_stat(txn, dbi, &st));
	return st.ms_entries;
}

/* Prints all pairs of dbi (printable keys and data, sizes otherwise). */
static void trace_all(const char *what, MDB_txn *txn, MDB_dbi dbi)
{
	MDB_cursor *c;
	MDB_val k, d;
	int rc, n = 0;

	OK(mdb_cursor_open(txn, dbi, &c));
	while ((rc = mdb_cursor_get(c, &k, &d, MDB_NEXT)) == 0) {
		if (d.mv_size <= 64)
			T("%s: %.*s = %.*s", what, (int)k.mv_size, (char *)k.mv_data,
				(int)d.mv_size, (char *)d.mv_data);
		else
			T("%s: %.*s = (%zu bytes)", what, (int)k.mv_size, (char *)k.mv_data, d.mv_size);
		n++;
	}
	RC(rc, MDB_NOTFOUND);
	T("%s: %d pairs", what, n);
	mdb_cursor_close(c);
}

/* ---------------------------------------------------------------------- */

static void test_txns(void)
{
	MDB_env *env = new_env("txns", 1UL << 22, 4, 0), *ro;
	MDB_txn *txn, *r1, *r2;
	MDB_envinfo info;
	MDB_dbi dbi;
	MDB_val k, d;
	size_t id;
	char path[1200];

	RC(mdb_txn_commit(NULL), EINVAL);
	mdb_txn_abort(NULL);

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	id = mdb_txn_id(txn);
	CHECK(id == 1);
	CHECK(mdb_txn_env(txn) == env);
	/* reset/renew are for read-only transactions */
	mdb_txn_reset(txn);
	RC(mdb_txn_renew(txn), EINVAL);
	CHECK(mdb_txn_id(txn) == id);
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	k = sv("k");
	d = sv("v1");
	OK(mdb_put(txn, dbi, &k, &d, 0));
	/* the transaction sees its own changes */
	OK(mdb_get(txn, dbi, &k, &d));
	CHECK(veq(d, "v1"));
	/* map size cannot change while a write transaction is active */
	RC(mdb_env_set_mapsize(env, 1UL << 23), EINVAL);
	/* readers do not */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r1));
	RC(mdb_get(r1, dbi, &k, &d), MDB_NOTFOUND);
	OK(mdb_txn_commit(txn));
	OK(mdb_env_info(env, &info));
	CHECK(info.me_last_txnid == 1);
	RC(mdb_get(r1, dbi, &k, &d), MDB_NOTFOUND);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r2));
	OK(mdb_get(r2, dbi, &k, &d));
	CHECK(veq(d, "v1"));
	mdb_txn_abort(r1);
	mdb_txn_abort(r2);

	/* an abort discards everything */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_txn_id(txn) == 2);
	d = sv("v2");
	OK(mdb_put(txn, dbi, &k, &d, 0));
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r1));
	OK(mdb_get(r1, dbi, &k, &d));
	CHECK(veq(d, "v1"));
	mdb_txn_abort(r1);

	/* a commit without changes writes nothing: the next id is the same */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_txn_id(txn) == 2);
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_txn_id(txn) == 2);
	OK(mdb_env_info(env, &info));
	CHECK(info.me_last_txnid == 1);
	mdb_txn_abort(txn);
	OK(mdb_env_set_mapsize(env, 1UL << 23));
	mdb_env_close(env);

	/* write transactions in a read-only environment: EACCES */
	snprintf(path, sizeof(path), "%s/txns", scratch);
	OK(mdb_env_create(&ro));
	OK(mdb_env_open(ro, path, MDB_RDONLY, 0664));
	RC(mdb_txn_begin(ro, NULL, 0, &txn), EACCES);
	OK(mdb_txn_begin(ro, NULL, MDB_RDONLY, &r1));
	OK(mdb_dbi_open(r1, NULL, 0, &dbi));
	OK(mdb_get(r1, dbi, &k, &d));
	CHECK(veq(d, "v1"));
	mdb_txn_abort(r1);
	mdb_env_close(ro);
}

/* ---------------------------------------------------------------------- */

static void test_dbi(void)
{
	MDB_env *env = new_env("dbi", 1UL << 22, 3, 0);
	MDB_txn *txn, *r;
	MDB_dbi main, a, b, c, gone, x;
	MDB_val k, d;
	unsigned flags;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &main));
	CHECK(main == MAIN_DBI);
	RC(mdb_dbi_open(txn, "a", 0, &a), MDB_NOTFOUND);
	RC(mdb_dbi_open(txn, "a", 0x10000000, &a), EINVAL);
	OK(mdb_dbi_open(txn, "a", MDB_CREATE | MDB_DUPSORT, &a));
	OK(mdb_dbi_open(txn, "a", 0, &x));
	CHECK(x == a);
	OK(mdb_dbi_flags(txn, a, &flags));
	CHECK(flags == MDB_DUPSORT);
	k = sv("k");
	d = sv("d1");
	OK(mdb_put(txn, a, &k, &d, 0));
	/* the main DB now holds the record of "a" */
	CHECK(entries(txn, main) == 1);
	/* a plain key in the main DB is not a database */
	k = sv("plain");
	d = sv("data");
	OK(mdb_put(txn, main, &k, &d, 0));
	RC(mdb_dbi_open(txn, "plain", 0, &x), MDB_INCOMPATIBLE);
	RC(mdb_dbi_open(txn, "plain", MDB_CREATE, &x), MDB_INCOMPATIBLE);
	OK(mdb_dbi_open(txn, "b", MDB_CREATE | MDB_INTEGERKEY, &b));
	OK(mdb_dbi_open(txn, "c", MDB_CREATE, &c));
	RC(mdb_dbi_open(txn, "d", MDB_CREATE, &x), MDB_DBS_FULL);
	OK(mdb_txn_commit(txn));

	/* handles created by an aborted transaction are closed */
	mdb_dbi_close(env, c);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "gone", MDB_CREATE, &gone));
	k = sv("k");
	d = sv("v");
	OK(mdb_put(txn, gone, &k, &d, 0));
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	RC(mdb_put(txn, gone, &k, &d, 0), EINVAL);
	RC(mdb_dbi_open(txn, "gone", 0, &x), MDB_NOTFOUND);
	/* committed ones are visible to later read transactions */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_dbi_flags(r, b, &flags));
	CHECK(flags == MDB_INTEGERKEY);
	k = sv("k");
	OK(mdb_get(r, a, &k, &d));
	CHECK(veq(d, "d1"));
	mdb_txn_abort(r);
	mdb_txn_abort(txn);

	/* a new database, then a read transaction after the commit */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "c2", MDB_CREATE | MDB_REVERSEKEY, &c));
	k = sv("ab");
	d = sv("1");
	OK(mdb_put(txn, c, &k, &d, 0));
	k = sv("ba");
	OK(mdb_put(txn, c, &k, &d, 0));
	trace_all("c2 (reverse keys)", txn, c);
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	trace_all("c2 after commit", r, c);
	trace_stat("main", r, main);
	mdb_txn_abort(r);

	/* The handle of an existing database, opened by a write transaction
	 * that aborts: C closes it, Rust keeps it. */
	mdb_dbi_close(env, b);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "b", 0, &b));
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	DIFFERS(mdb_dbi_flags(r, b, &flags), EINVAL, 0);
	mdb_txn_abort(r);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

static void test_put_get_del(void)
{
	MDB_env *env = new_env("put", 1UL << 24, 8, 0);
	MDB_txn *txn, *r;
	MDB_dbi db, dups, fixed, ikey;
	MDB_val k, d, k2;
	char key[600], buf[100000];
	unsigned int u;
	size_t n;
	int i;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "plain", MDB_CREATE, &db));
	OK(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dups));
	OK(mdb_dbi_open(txn, "fixed", MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED, &fixed));
	OK(mdb_dbi_open(txn, "ikey", MDB_CREATE | MDB_INTEGERKEY, &ikey));

	/* argument checks */
	k = sv("a");
	d = sv("1");
	RC(mdb_put(NULL, db, &k, &d, 0), EINVAL);
	RC(mdb_put(txn, db, NULL, &d, 0), EINVAL);
	RC(mdb_put(txn, db, &k, NULL, 0), EINVAL);
	RC(mdb_put(txn, 99, &k, &d, 0), EINVAL);
	RC(mdb_put(txn, FREE_DBI, &k, &d, 0), EINVAL);
	RC(mdb_put(txn, db, &k, &d, MDB_CURRENT), EINVAL);
	RC(mdb_put(txn, db, &k, &d, MDB_MULTIPLE), EINVAL);
	RC(mdb_del(txn, db, NULL, NULL), EINVAL);
	RC(mdb_del(txn, 99, &k, NULL), EINVAL);
	RC(mdb_del(txn, FREE_DBI, &k, NULL), EINVAL);
	/* key and data sizes */
	memset(key, 'k', sizeof(key));
	k = bv(key, 0);
	RC(mdb_put(txn, db, &k, &d, 0), MDB_BAD_VALSIZE);
	RC(mdb_del(txn, db, &k, NULL), MDB_BAD_VALSIZE);
	k = bv(key, 512);
	RC(mdb_put(txn, db, &k, &d, 0), MDB_BAD_VALSIZE);
	k = bv(key, 511);
	OK(mdb_put(txn, db, &k, &d, 0));
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	k = sv("a");
	d = bv(key, 0);
	RC(mdb_put(txn, dups, &k, &d, 0), MDB_BAD_VALSIZE);
	d = bv(key, 512);
	RC(mdb_put(txn, dups, &k, &d, 0), MDB_BAD_VALSIZE);
	d = bv(key, 0);
	OK(mdb_put(txn, db, &k, &d, 0));	/* empty data is fine without DUPSORT */
	OK(mdb_get(txn, db, &k, &d));
	CHECK(d.mv_size == 0);

	/* overwrite, MDB_NOOVERWRITE returns the existing data */
	k = sv("b");
	d = sv("first");
	OK(mdb_put(txn, db, &k, &d, 0));
	d = sv("second");
	OK(mdb_put(txn, db, &k, &d, 0));
	d = sv("third");
	RC(mdb_put(txn, db, &k, &d, MDB_NOOVERWRITE), MDB_KEYEXIST);
	CHECK(veq(d, "second"));
	/* MDB_NODUPDATA without DUPSORT: ignored, overwrites */
	d = sv("fourth");
	OK(mdb_put(txn, db, &k, &d, MDB_NODUPDATA));
	OK(mdb_get(txn, db, &k, &d));
	CHECK(veq(d, "fourth"));
	k = sv("nope");
	RC(mdb_get(txn, db, &k, &d), MDB_NOTFOUND);

	/* MDB_APPEND */
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (u = 0; u < 300; u++) {
		k = bv(&u, sizeof(u));
		snprintf(buf, sizeof(buf), "v%u", u);
		d = sv(buf);
		OK(mdb_put(txn, ikey, &k, &d, MDB_APPEND));
	}
	u = 299;
	k = bv(&u, sizeof(u));
	d = sv("x");
	RC(mdb_put(txn, ikey, &k, &d, MDB_APPEND), MDB_KEYEXIST);
	CHECK(veq(d, "x"));
	RC(mdb_put(txn, ikey, &k, &d, MDB_APPEND | MDB_NOOVERWRITE), MDB_KEYEXIST);
	CHECK(veq(d, "x"));
	u = 5;
	RC(mdb_put(txn, ikey, &k, &d, MDB_APPEND), MDB_KEYEXIST);
	trace_stat("ikey after append", txn, ikey);

	/* duplicates: NODUPDATA, APPENDDUP, NOOVERWRITE on DUPSORT */
	k = sv("dk");
	for (i = 0; i < 5; i++) {
		snprintf(buf, sizeof(buf), "d%d", i);
		d = sv(buf);
		OK(mdb_put(txn, dups, &k, &d, MDB_APPENDDUP));
	}
	d = sv("d2");
	RC(mdb_put(txn, dups, &k, &d, MDB_NODUPDATA), MDB_KEYEXIST);
	CHECK(veq(d, "d2"));
	OK(mdb_put(txn, dups, &k, &d, 0));	/* the pair exists: no change */
	d = sv("d1");
	RC(mdb_put(txn, dups, &k, &d, MDB_APPENDDUP), MDB_KEYEXIST);
	d = sv("zz");
	RC(mdb_put(txn, dups, &k, &d, MDB_NOOVERWRITE), MDB_KEYEXIST);
	CHECK(veq(d, "d0"));	/* the first duplicate */
	CHECK(entries(txn, dups) == 5);
	/* MDB_RESERVE is not for DUPSORT (C does not check, see KNOWN_C_BUGS.md #16) */
	d.mv_size = 4;
	DEVIATION(RC(mdb_put(txn, dups, &k, &d, MDB_RESERVE), EINVAL));

	/* MDB_RESERVE: inline and on overflow pages */
	k = sv("reserved");
	d.mv_size = 100;
	d.mv_data = NULL;
	OK(mdb_put(txn, db, &k, &d, MDB_RESERVE));
	CHECK(d.mv_data != NULL && d.mv_size == 100);
	if (d.mv_data)
		memset(d.mv_data, 'r', d.mv_size);
	k = sv("reserved-big");
	d.mv_size = 50000;
	d.mv_data = NULL;
	OK(mdb_put(txn, db, &k, &d, MDB_RESERVE));
	CHECK(d.mv_data != NULL);
	if (d.mv_data)
		memset(d.mv_data, 'R', d.mv_size);
	k = sv("reserved");
	OK(mdb_get(txn, db, &k, &d));
	memset(buf, 'r', 100);
	CHECK(d.mv_size == 100 && !memcmp(d.mv_data, buf, 100));
	k = sv("reserved");
	d.mv_size = 10;
	RC(mdb_put(txn, db, &k, &d, MDB_RESERVE | MDB_NOOVERWRITE), MDB_KEYEXIST);
	CHECK(d.mv_size == 100 && !memcmp(d.mv_data, buf, 100));

	/* big values: overflow pages, overwritten bigger, smaller, inline */
	k = sv("big");
	memset(buf, 'B', sizeof(buf));
	d = bv(buf, 40000);
	OK(mdb_put(txn, db, &k, &d, 0));
	trace_stat("plain with a 40000-byte value", txn, db);
	d = bv(buf, 90000);
	OK(mdb_put(txn, db, &k, &d, 0));
	trace_stat("overwritten with 90000 bytes", txn, db);
	d = bv(buf, 20000);
	OK(mdb_put(txn, db, &k, &d, 0));
	trace_stat("overwritten with 20000 bytes", txn, db);
	OK(mdb_get(txn, db, &k, &d));
	CHECK(d.mv_size == 20000 && !memcmp(d.mv_data, buf, 20000));
	d = sv("small");
	OK(mdb_put(txn, db, &k, &d, 0));
	trace_stat("overwritten with 5 bytes", txn, db);
	OK(mdb_txn_commit(txn));

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	k = sv("reserved-big");
	OK(mdb_get(r, db, &k, &d));
	CHECK(d.mv_size == 50000 && ((char *)d.mv_data)[0] == 'R' &&
		((char *)d.mv_data)[49999] == 'R');
	trace_all("plain", r, db);
	trace_all("dups", r, dups);
	mdb_txn_abort(r);

	/* deletes */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	k = sv("b");
	d = sv("ignored");
	OK(mdb_del(txn, db, &k, &d));	/* data is ignored without DUPSORT */
	RC(mdb_del(txn, db, &k, NULL), MDB_NOTFOUND);
	k = sv("dk");
	d = sv("d3");
	OK(mdb_del(txn, dups, &k, &d));
	RC(mdb_del(txn, dups, &k, &d), MDB_NOTFOUND);
	CHECK(entries(txn, dups) == 4);
	OK(mdb_del(txn, dups, &k, NULL));
	CHECK(entries(txn, dups) == 0);
	RC(mdb_del(txn, dups, &k, NULL), MDB_NOTFOUND);
	/* keys and data that point into the transaction's own pages */
	k = sv("reserved");
	OK(mdb_get(txn, db, &k, &d));
	k2 = sv("copy-of-reserved");
	OK(mdb_put(txn, db, &k2, &d, 0));
	OK(mdb_get(txn, db, &k2, &d));
	CHECK(d.mv_size == 100 && ((char *)d.mv_data)[99] == 'r');
	{
		MDB_cursor *c;
		OK(mdb_cursor_open(txn, ikey, &c));
		n = 0;
		while (mdb_cursor_get(c, &k, &d, MDB_NEXT) == 0) {
			if (n++ % 3 == 0)
				OK(mdb_del(txn, ikey, &k, NULL));	/* as mtest.c does */
		}
		mdb_cursor_close(c);
	}
	CHECK(entries(txn, ikey) == 200);
	trace_stat("ikey after deleting every third key", txn, ikey);
	OK(mdb_txn_commit(txn));

	/* MDB_DUPFIXED with mismatched sizes: refused (KNOWN_C_BUGS.md #12) */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	k = sv("f");
	u = 1;
	d = bv(&u, sizeof(u));
	OK(mdb_put(txn, fixed, &k, &d, 0));
	d = bv(buf, 8);
	DEVIATION(RC(mdb_put(txn, fixed, &k, &d, 0), MDB_BAD_VALSIZE));
	mdb_txn_abort(txn);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

static void test_cursors(void)
{
	MDB_env *env = new_env("cursors", 1UL << 24, 8, 0);
	MDB_txn *txn, *r;
	MDB_dbi db, dups, fixed;
	MDB_cursor *c, *c2, *rcur;
	MDB_val k, d, m[2];
	char buf[64];
	unsigned int items[300];
	size_t cnt;
	int i, rc;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "db", MDB_CREATE, &db));
	OK(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dups));
	OK(mdb_dbi_open(txn, "fixed", MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP, &fixed));
	RC(mdb_cursor_open(txn, FREE_DBI, &c), EINVAL);
	RC(mdb_cursor_open(txn, 99, &c), EINVAL);
	RC(mdb_cursor_open(txn, db, NULL), EINVAL);
	OK(mdb_cursor_open(txn, db, &c));
	CHECK(mdb_cursor_txn(c) == txn);
	CHECK(mdb_cursor_dbi(c) == db);
	/* write cursors cannot be renewed; read cursors not onto write txns */
	RC(mdb_cursor_renew(txn, c), EINVAL);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	RC(mdb_cursor_open(r, db, &rcur), EINVAL);	/* not committed yet */
	mdb_txn_abort(r);

	/* empty database */
	RC(mdb_cursor_get(c, &k, &d, MDB_FIRST), MDB_NOTFOUND);
	RC(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT), EINVAL);
	RC(mdb_cursor_del(c, 0), EINVAL);
	k = sv("x");
	d = sv("y");
	RC(mdb_cursor_put(c, &k, &d, MDB_CURRENT), EINVAL);
	RC(mdb_cursor_put(NULL, &k, &d, 0), EINVAL);
	RC(mdb_cursor_put(c, NULL, &d, 0), EINVAL);

	/* puts leave the cursor on the new item */
	for (i = 0; i < 100; i++) {
		snprintf(buf, sizeof(buf), "key%03d", i * 2);
		k = sv(buf);
		d = sv(buf + 3);
		OK(mdb_cursor_put(c, &k, &d, 0));
		OK(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT));
		CHECK(veq(k, buf) && veq(d, buf + 3));
	}
	k = sv("key053");
	d = sv("new");
	OK(mdb_cursor_put(c, &k, &d, MDB_NOOVERWRITE));
	k = sv("key053");
	d = sv("newer");
	RC(mdb_cursor_put(c, &k, &d, MDB_NOOVERWRITE), MDB_KEYEXIST);
	CHECK(veq(d, "new"));
	OK(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key053") && veq(d, "new"));
	/* MDB_CURRENT: same size, bigger, smaller */
	d = sv("NEW");
	OK(mdb_cursor_put(c, &k, &d, MDB_CURRENT));
	d = sv("a much longer value than before");
	OK(mdb_cursor_put(c, &k, &d, MDB_CURRENT));
	OK(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key053") && veq(d, "a much longer value than before"));
	d = sv("s");
	OK(mdb_cursor_put(c, &k, &d, MDB_CURRENT));
	k = sv("key053");
	OK(mdb_get(txn, db, &k, &d));
	CHECK(veq(d, "s"));
	/* MDB_RESERVE through a cursor */
	k = sv("key051");
	d.mv_size = 6;
	OK(mdb_cursor_put(c, &k, &d, MDB_RESERVE));
	memcpy(d.mv_data, "filled", 6);
	OK(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key051") && veq(d, "filled"));
	/* unknown flags are ignored by mdb_cursor_put, as in C */
	k = sv("key052");
	d = sv("flags");
	OK(mdb_cursor_put(c, &k, &d, 0x1000000));

	/* deletes while other cursors are positioned (cursor fixups) */
	OK(mdb_cursor_open(txn, db, &c2));
	k = sv("key100");
	OK(mdb_cursor_get(c2, &k, &d, MDB_SET));
	k = sv("key098");
	OK(mdb_cursor_get(c, &k, &d, MDB_SET));
	OK(mdb_cursor_del(c, 0));
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key100"));
	/* deleting c2's item moves c2 to the next one */
	OK(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key100"));
	OK(mdb_cursor_del(c, 0));
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key102"));
	OK(mdb_cursor_get(c2, &k, &d, MDB_NEXT));
	T("after deleting its item, MDB_NEXT gives %.*s", (int)k.mv_size, (char *)k.mv_data);
	OK(mdb_cursor_get(c2, &k, &d, MDB_PREV));
	T("then MDB_PREV gives %.*s", (int)k.mv_size, (char *)k.mv_data);
	/* delete every other item with one cursor while the other walks */
	OK(mdb_cursor_get(c, &k, &d, MDB_FIRST));
	i = 0;
	do {
		if (i++ % 2 == 0)
			OK(mdb_cursor_del(c, 0));
	} while ((rc = mdb_cursor_get(c, &k, &d, MDB_NEXT)) == 0);
	RC(rc, MDB_NOTFOUND);
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_CURRENT));
	T("c2 after the sweep: %.*s", (int)k.mv_size, (char *)k.mv_data);
	trace_stat("db after the sweep", txn, db);
	trace_all("db", txn, db);
	RC(mdb_cursor_count(c, &cnt), MDB_INCOMPATIBLE);
	mdb_cursor_close(c2);

	/* DUPSORT: sub-page, then sub-DB, counts, MDB_CURRENT, NODUPDATA */
	OK(mdb_cursor_open(txn, dups, &c2));
	k = sv("a");
	for (i = 0; i < 3; i++) {
		snprintf(buf, sizeof(buf), "%c1", 'a' + i);
		d = sv(buf);
		OK(mdb_cursor_put(c2, &k, &d, 0));
	}
	OK(mdb_cursor_count(c2, &cnt));
	CHECK(cnt == 3);
	d = sv("b1");
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_BOTH));
	d = sv("b2");
	OK(mdb_cursor_put(c2, &k, &d, MDB_CURRENT));
	k = sv("sentinel");
	OK(mdb_cursor_get(c2, &k, &d, MDB_LAST_DUP));
	CHECK(veq(k, "sentinel") && veq(d, "c1"));	/* key untouched (C) */
	trace_all("dups after MDB_CURRENT", txn, dups);
	trace_stat("dups on a sub-page", txn, dups);
	/* MDB_CURRENT replacing a key's only item by one of the maximum size
	 * under a key of the maximum size: at 0.9's page sizes (the OS page
	 * size) it fits a node, so C and Rust replace it in place. (Where it
	 * does not, with pages of 2 KiB, C stores it on overflow pages and
	 * rlmdb deletes and inserts: KNOWN_C_BUGS.md #38.) */
	{
		char kmax[511], dmax[511];
		memset(kmax, 'K', sizeof(kmax));
		memset(dmax, 'D', sizeof(dmax));
		k = bv(kmax, sizeof(kmax));
		d = sv("one");
		OK(mdb_cursor_put(c2, &k, &d, 0));
		d = bv(dmax, sizeof(dmax));
		OK(mdb_cursor_put(c2, &k, &d, MDB_CURRENT));
		trace_stat("dups after MDB_CURRENT of 511 bytes", txn, dups);
		d = sv("two");
		OK(mdb_cursor_put(c2, &k, &d, 0));
		OK(mdb_cursor_count(c2, &cnt));
		CHECK(cnt == 2);
		trace_stat("dups with a second duplicate of it", txn, dups);
		OK(mdb_del(txn, dups, &k, NULL));
	}
	k = sv("m");
	for (i = 0; i < 600; i++) {
		snprintf(buf, sizeof(buf), "dup-value-%05d-padding-padding", i);
		d = sv(buf);
		OK(mdb_cursor_put(c2, &k, &d, 0));
		if (i == 20)
			trace_stat("dups, 21 values of m", txn, dups);
	}
	trace_stat("dups, 600 values of m (sub-DB)", txn, dups);
	OK(mdb_cursor_count(c2, &cnt));
	CHECK(cnt == 600);
	d = sv("dup-value-00100-padding-padding");
	RC(mdb_cursor_put(c2, &k, &d, MDB_NODUPDATA), MDB_KEYEXIST);
	k = sv("z");
	d = sv("last");
	OK(mdb_cursor_put(c2, &k, &d, 0));
	/* delete half of m's values with the cursor */
	k = sv("m");
	OK(mdb_cursor_get(c2, &k, &d, MDB_SET));
	for (i = 0; i < 300; i++) {
		OK(mdb_cursor_del(c2, 0));
		if (i % 2 == 0)
			OK(mdb_cursor_get(c2, &k, &d, MDB_NEXT_DUP));
	}
	OK(mdb_cursor_count(c2, &cnt));
	T("m has %zu values left", cnt);
	trace_stat("dups after deleting 300 values of m", txn, dups);
	/* MDB_NODUPDATA deletes all of the key's values */
	OK(mdb_cursor_del(c2, MDB_NODUPDATA));
	CHECK(entries(txn, dups) == 4);
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "z"));
	trace_stat("dups after MDB_NODUPDATA", txn, dups);
	mdb_cursor_close(c2);

	/* MDB_MULTIPLE on MDB_DUPFIXED */
	OK(mdb_cursor_open(txn, fixed, &c2));
	for (i = 0; i < 300; i++)
		items[i] = (unsigned)(i * 3);
	k = sv("f");
	m[0] = bv(items, sizeof(items[0]));
	m[1].mv_size = 300;
	m[1].mv_data = NULL;
	OK(mdb_cursor_put(c2, &k, m, MDB_MULTIPLE));
	CHECK(m[1].mv_size == 300);
	CHECK(m[0].mv_data == (void *)&items[299]);
	OK(mdb_cursor_count(c2, &cnt));
	CHECK(cnt == 300);
	trace_stat("fixed after 300 MDB_MULTIPLE items", txn, fixed);
	/* again, half of them present already */
	for (i = 0; i < 300; i++)
		items[i] = (unsigned)(i * 3 + (i % 2));
	m[0] = bv(items, sizeof(items[0]));
	m[1].mv_size = 300;
	OK(mdb_cursor_put(c2, &k, m, MDB_MULTIPLE));
	CHECK(m[1].mv_size == 300);
	OK(mdb_cursor_count(c2, &cnt));
	T("fixed: %zu values after adding 300 with 150 duplicates", cnt);
	OK(mdb_cursor_get(c2, &k, &d, MDB_FIRST));
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_MULTIPLE));
	T("MDB_GET_MULTIPLE: %zu bytes, first %u", d.mv_size, *(unsigned *)d.mv_data);
	m[1].mv_size = 0;
	RC(mdb_cursor_put(c2, &k, m, MDB_MULTIPLE), EINVAL);
	m[0].mv_size = 0;
	m[1].mv_size = 2;
	RC(mdb_cursor_put(c2, &k, m, MDB_MULTIPLE), MDB_BAD_VALSIZE);
	CHECK(m[1].mv_size == 0);
	mdb_cursor_close(c2);
	OK(mdb_cursor_open(txn, db, &c2));
	m[0] = bv(items, 4);
	m[1].mv_size = 2;
	RC(mdb_cursor_put(c2, &k, m, MDB_MULTIPLE), MDB_INCOMPATIBLE);
	/* a cursor of a write transaction may be closed before the commit, or
	 * left to the commit (it frees it) */
	OK(mdb_txn_commit(txn));

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	trace_all("db committed", r, db);
	trace_stat("dups committed", r, dups);
	trace_stat("fixed committed", r, fixed);
	OK(mdb_cursor_open(r, fixed, &rcur));
	OK(mdb_cursor_get(rcur, &k, &d, MDB_FIRST));
	OK(mdb_cursor_count(rcur, &cnt));
	T("fixed committed: %zu values", cnt);
	mdb_txn_abort(r);
	mdb_cursor_close(rcur);

	/* writes through a read-only transaction's cursor */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_cursor_open(r, db, &rcur));
	k = sv("q");
	d = sv("q");
	RC(mdb_cursor_put(rcur, &k, &d, 0), EACCES);
	RC(mdb_cursor_del(rcur, 0), EACCES);
	mdb_cursor_close(rcur);
	mdb_txn_abort(r);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

/* MDB_DUPFIXED items of 1, 2 and 3 bytes. A P_LEAF2 (sub-)page counts its
 * items in mp_lower/mp_upper, mp_upper dropping by ksize - sizeof(indx_t)
 * per item: unsigned in mdb_node_add, so for 1-byte items mp_upper grows
 * past the end of the page. The Rust library refused such an insert with
 * MDB_PAGE_FULL; the 2- and 3-byte databases guard the neighbours. */
static void test_dupfixed_small(void)
{
	MDB_env *env = new_env("small", 1UL << 22, 8, 0);
	MDB_txn *txn, *r;
	MDB_cursor *c;
	MDB_dbi db;
	MDB_val k, d, m[2];
	unsigned char v[3], items[3 * 256], prev;
	char name[32];
	size_t size, cnt, total;
	int i, rc;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (size = 1; size <= 3; size++) {
		snprintf(name, sizeof(name), "fixed%zu", size);
		OK(mdb_dbi_open(txn, name, MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED, &db));
		/* "a": 200 single puts in a scrambled order (167 is odd, so
		 * i * 167 mod 256 visits every byte once) */
		k = sv("a");
		memset(v, 'x', sizeof(v));
		for (i = 0; i < 200; i++) {
			v[0] = (unsigned char)(i * 167);
			d = bv(v, size);
			OK(mdb_put(txn, db, &k, &d, 0));
		}
		snprintf(name, sizeof(name), "fixed%zu after 200 puts", size);
		trace_stat(name, txn, db);
		/* "b": all 256 first bytes at once */
		for (i = 0; i < 256; i++) {
			memset(items + i * size, 'y', size);
			items[i * size] = (unsigned char)(255 - i);
		}
		OK(mdb_cursor_open(txn, db, &c));
		k = sv("b");
		m[0] = bv(items, size);
		m[1].mv_size = 256;
		m[1].mv_data = NULL;
		OK(mdb_cursor_put(c, &k, m, MDB_MULTIPLE));
		CHECK(m[1].mv_size == 256);
		OK(mdb_cursor_count(c, &cnt));
		CHECK(cnt == 256);
		/* every third value of "a" goes again */
		k = sv("a");
		for (i = 0; i < 200; i += 3) {
			v[0] = (unsigned char)(i * 167);
			d = bv(v, size);
			OK(mdb_del(txn, db, &k, &d));
		}
		snprintf(name, sizeof(name), "fixed%zu after deletes", size);
		trace_stat(name, txn, db);
		/* the values of "a" are sorted, one item each */
		OK(mdb_cursor_get(c, &k, &d, MDB_SET));
		OK(mdb_cursor_get(c, &k, &d, MDB_FIRST_DUP));
		CHECK(d.mv_size == size);
		prev = *(unsigned char *)d.mv_data;
		cnt = 1;
		while ((rc = mdb_cursor_get(c, &k, &d, MDB_NEXT_DUP)) == 0) {
			CHECK(d.mv_size == size && *(unsigned char *)d.mv_data > prev);
			prev = *(unsigned char *)d.mv_data;
			cnt++;
		}
		RC(rc, MDB_NOTFOUND);
		T("fixed%zu: a has %zu values", size, cnt);
		/* MDB_GET_MULTIPLE / MDB_NEXT_MULTIPLE return whole items */
		k = sv("b");
		OK(mdb_cursor_get(c, &k, &d, MDB_SET));
		OK(mdb_cursor_get(c, &k, &d, MDB_GET_MULTIPLE));
		total = 0;
		do {
			CHECK(d.mv_size % size == 0);
			T("fixed%zu: multiple %zu bytes, first %u", size, d.mv_size,
				*(unsigned char *)d.mv_data);
			total += d.mv_size;
		} while ((rc = mdb_cursor_get(c, &k, &d, MDB_NEXT_MULTIPLE)) == 0);
		RC(rc, MDB_NOTFOUND);
		CHECK(total == 256 * size);
		mdb_cursor_close(c);
	}
	OK(mdb_txn_commit(txn));

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	for (size = 1; size <= 3; size++) {
		snprintf(name, sizeof(name), "fixed%zu", size);
		OK(mdb_dbi_open(r, name, 0, &db));
		snprintf(name, sizeof(name), "fixed%zu committed", size);
		trace_stat(name, r, db);
	}
	mdb_txn_abort(r);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

static void test_comparators(void)
{
	MDB_env *env = new_env("cmp", 1UL << 22, 4, 0);
	MDB_txn *txn, *r;
	MDB_dbi num, rev;
	MDB_val k, d;
	char buf[32];
	int i, rc;

	/* Custom orders installed in the transaction that creates the DBs
	 * and used there. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "num", MDB_CREATE, &num));
	OK(mdb_set_compare(txn, num, numcmp));
	OK(mdb_dbi_open(txn, "rev", MDB_CREATE | MDB_DUPSORT, &rev));
	OK(mdb_set_dupsort(txn, rev, revcmp));
	k = sv("10");
	d = sv("x");
	OK(mdb_put(txn, num, &k, &d, 0));
	k = sv("9");
	OK(mdb_put(txn, num, &k, &d, 0));
	{
		MDB_cursor *c;
		OK(mdb_cursor_open(txn, num, &c));
		OK(mdb_cursor_get(c, &k, &d, MDB_FIRST));
		CHECK(veq(k, "9"));
		mdb_cursor_close(c);
	}
	mdb_txn_abort(txn);

	/* Installed first, used in the next transactions (works in both). */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "num", MDB_CREATE, &num));
	OK(mdb_dbi_open(txn, "rev", MDB_CREATE | MDB_DUPSORT, &rev));
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_set_compare(r, num, numcmp));
	OK(mdb_set_dupsort(r, rev, revcmp));
	mdb_txn_abort(r);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (i = 0; i < 500; i++) {
		snprintf(buf, sizeof(buf), "%d", (i * 7919) % 1000);
		k = sv(buf);
		d = sv(buf);
		OK(mdb_put(txn, num, &k, &d, 0));
		k = sv("k");
		OK(mdb_put(txn, rev, &k, &d, 0));
	}
	k = sv("100");
	d = sv("y");
	RC(mdb_put(txn, num, &k, &d, MDB_APPEND), MDB_KEYEXIST);
	k = sv("1000");
	OK(mdb_put(txn, num, &k, &d, MDB_APPEND));
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	{
		MDB_cursor *c;
		long prev = -1;
		int n = 0, sorted = 1;
		OK(mdb_cursor_open(r, num, &c));
		while ((rc = mdb_cursor_get(c, &k, &d, MDB_NEXT)) == 0) {
			long v = numval(&k);
			if (v <= prev)
				sorted = 0;
			prev = v;
			n++;
		}
		RC(rc, MDB_NOTFOUND);
		CHECK(sorted && n == 501);
		T("num: %d keys in numeric order, last %ld", n, prev);
		mdb_cursor_close(c);
		OK(mdb_cursor_open(r, rev, &c));
		k = sv("k");
		OK(mdb_cursor_get(c, &k, &d, MDB_SET));
		T("rev: first duplicate %.*s", (int)d.mv_size, (char *)d.mv_data);
		OK(mdb_cursor_get(c, &k, &d, MDB_LAST_DUP));
		T("rev: last duplicate %.*s", (int)d.mv_size, (char *)d.mv_data);
		mdb_cursor_close(c);
	}
	trace_stat("num", r, num);
	trace_stat("rev", r, rev);
	mdb_txn_abort(r);
	mdb_env_close(env);
}

/* mdb_set_compare (dup 0) or mdb_set_dupsort (dup 1) on the handle of a
 * database whose creation is then aborted, by the creating transaction or
 * (nested 1) by the child that created it. The database created next gets
 * the same handle and, with default flags, the default order: in mdb_cmp
 * or mdb_dcmp and in what it stores. */
static void comparator_reuse(int dup, int nested)
{
	MDB_env *env;
	MDB_txn *parent = NULL, *txn, *r;
	MDB_dbi gone, dbi;
	MDB_cursor *c;
	MDB_val k, d, a = sv("a"), b = sv("b");
	unsigned flags = dup ? MDB_DUPSORT : 0;
	int (*cmp)(MDB_txn *, MDB_dbi, const MDB_val *, const MDB_val *) =
		dup ? mdb_dcmp : mdb_cmp;
	char name[32];

	snprintf(name, sizeof(name), "cmpreuse-%d-%d", dup, nested);
	env = new_env(name, 1UL << 22, 2, 0);
	if (nested)
		OK(mdb_txn_begin(env, NULL, 0, &parent));
	OK(mdb_txn_begin(env, parent, 0, &txn));
	OK(mdb_dbi_open(txn, "gone", MDB_CREATE | flags, &gone));
	if (dup)
		OK(mdb_set_dupsort(txn, gone, revcmp));
	else
		OK(mdb_set_compare(txn, gone, revcmp));
	CHECK(cmp(txn, gone, &a, &b) > 0);
	mdb_txn_abort(txn);

	if (nested)
		txn = parent;
	else
		OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "new", MDB_CREATE | flags, &dbi));
	CHECK(dbi == gone);
	CHECK(cmp(txn, dbi, &a, &b) < 0);
	k = dup ? sv("k") : b;
	d = b;
	OK(mdb_put(txn, dbi, &k, &d, 0));
	k = dup ? sv("k") : a;
	d = a;
	OK(mdb_put(txn, dbi, &k, &d, 0));
	OK(mdb_txn_commit(txn));

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	CHECK(cmp(r, dbi, &a, &b) < 0);
	OK(mdb_cursor_open(r, dbi, &c));
	OK(mdb_cursor_get(c, &k, &d, MDB_FIRST));
	CHECK(veq(d, "a"));
	T("%s: first %.*s", name, (int)d.mv_size, (char *)d.mv_data);
	OK(mdb_cursor_get(c, &k, &d, MDB_NEXT));
	CHECK(veq(d, "b"));
	mdb_cursor_close(c);
	mdb_txn_abort(r);
	mdb_env_close(env);
}

static void test_comparator_reuse(void)
{
	int dup, nested;

	for (dup = 0; dup < 2; dup++)
		for (nested = 0; nested < 2; nested++)
			comparator_reuse(dup, nested);
}

/* ---------------------------------------------------------------------- */

static void test_errors(void)
{
	MDB_env *env = new_env("full", 1UL << 18, 4, 0);
	MDB_txn *txn;
	MDB_dbi db;
	MDB_cursor *c;
	MDB_val k, d;
	MDB_stat st;
	char buf[2000];
	int i, rc = 0;

	OK(mdb_env_stat(env, &st));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &db));
	OK(mdb_cursor_open(txn, db, &c));
	memset(buf, 'x', sizeof(buf));
	for (i = 0; i < 100000; i++) {
		snprintf(buf, 16, "%08d", i);
		k = bv(buf, 8);
		d = bv(buf, sizeof(buf));
		if ((rc = mdb_put(txn, db, &k, &d, 0)) != 0)
			break;
	}
	RC(rc, MDB_MAP_FULL);
	T("MDB_MAP_FULL after %d puts", i);
	/* the transaction is unusable now */
	RC(mdb_put(txn, db, &k, &d, 0), MDB_BAD_TXN);
	RC(mdb_get(txn, db, &k, &d), MDB_BAD_TXN);
	RC(mdb_del(txn, db, &k, NULL), MDB_BAD_TXN);
	RC(mdb_cursor_get(c, &k, &d, MDB_FIRST), MDB_BAD_TXN);
	RC(mdb_cursor_put(c, &k, &d, 0), MDB_BAD_TXN);
	RC(mdb_cursor_del(c, 0), MDB_BAD_TXN);
	RC(mdb_stat(txn, db, &st), MDB_BAD_TXN);
	RC(mdb_dbi_open(txn, "x", 0, &db), MDB_BAD_TXN);
	/* and the commit fails, aborting and freeing it */
	RC(mdb_txn_commit(txn), MDB_BAD_TXN);
	/* nothing was written */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_txn_id(txn) == 1);
	OK(mdb_dbi_open(txn, NULL, 0, &db));
	CHECK(entries(txn, db) == 0);
	mdb_txn_abort(txn);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

struct writer { MDB_env *env; MDB_dbi dbi; int rc; size_t id; };

static void *second_writer(void *arg)
{
	struct writer *w = arg;
	MDB_txn *txn;
	MDB_val k = sv("thread"), d = sv("2");

	w->rc = mdb_txn_begin(w->env, NULL, 0, &txn);
	if (w->rc)
		return NULL;
	w->id = mdb_txn_id(txn);
	w->rc = mdb_put(txn, w->dbi, &k, &d, 0);
	if (!w->rc)
		w->rc = mdb_txn_commit(txn);
	else
		mdb_txn_abort(txn);
	return NULL;
}

/* The writer lock serialises write transactions of different threads; each
 * one ends on the thread that began it. */
static void test_threads(void)
{
	MDB_env *env = new_env("threads", 1UL << 22, 4, 0);
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val k = sv("thread"), d = sv("1");
	struct writer w;
	pthread_t th;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_put(txn, dbi, &k, &d, 0));
	w.env = env;
	w.dbi = dbi;
	w.rc = -1;
	w.id = 0;
	pthread_create(&th, NULL, second_writer, &w);
	usleep(100000);
	CHECK(w.id == 0);	/* still waiting for the lock */
	OK(mdb_txn_commit(txn));
	pthread_join(th, NULL);
	RC(w.rc, 0);
	CHECK(w.id == 2);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_get(txn, dbi, &k, &d));
	CHECK(veq(d, "2"));
	mdb_txn_abort(txn);
	mdb_env_close(env);
}


/* ---------------------------------------------------------------------- */

static void test_drop(void)
{
	MDB_env *env = new_env("drop", 1UL << 24, 8, 0);
	MDB_txn *txn, *r;
	MDB_dbi d1, d2, x;
	MDB_cursor *c;
	MDB_val k, d;
	char buf[8000];
	int i;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "d1", MDB_CREATE, &d1));
	OK(mdb_dbi_open(txn, "d2", MDB_CREATE | MDB_DUPSORT, &d2));
	memset(buf, 'v', sizeof(buf));
	for (i = 0; i < 1000; i++) {
		char key[16];
		snprintf(key, sizeof(key), "k%04d", i);
		k = sv(key);
		d = bv(buf, i % 100 == 0 ? sizeof(buf) : 20);
		OK(mdb_put(txn, d1, &k, &d, 0));
		k = sv(i % 2 ? "odd" : "even");
		d = sv(key);
		OK(mdb_put(txn, d2, &k, &d, 0));
	}
	trace_stat("d1 before mdb_drop", txn, d1);
	trace_stat("d2 before mdb_drop", txn, d2);
	OK(mdb_txn_commit(txn));

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	RC(mdb_drop(txn, d1, 2), EINVAL);
	RC(mdb_drop(txn, FREE_DBI, 0), EINVAL);
	RC(mdb_drop(txn, 99, 0), EINVAL);
	RC(mdb_drop(NULL, d1, 0), EINVAL);
	OK(mdb_cursor_open(txn, d1, &c));
	OK(mdb_cursor_get(c, &k, &d, MDB_FIRST));
	/* empty d1: the cursor becomes unpositioned, the handle stays */
	OK(mdb_drop(txn, d1, 0));
	CHECK(entries(txn, d1) == 0);
	RC(mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT), EINVAL);
	RC(mdb_cursor_get(c, &k, &d, MDB_FIRST), MDB_NOTFOUND);
	k = sv("again");
	d = sv("1");
	OK(mdb_put(txn, d1, &k, &d, 0));
	trace_stat("d1 after mdb_drop(0) and a put", txn, d1);
	/* delete d2: its handle is closed at once */
	OK(mdb_drop(txn, d2, 1));
	RC(mdb_dbi_open(txn, "d2", 0, &x), MDB_NOTFOUND);
	trace_stat("main after mdb_drop(1)", txn, MAIN_DBI);
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	trace_all("d1 committed", r, d1);
	RC(mdb_dbi_open(r, "d2", 0, &x), MDB_NOTFOUND);
	trace_stat("free DB after the drops", r, FREE_DBI);
	mdb_txn_abort(r);

	/* deleting in a transaction that aborts: the handle is closed anyway
	 * (C closes it in mdb_drop), the database stays */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_drop(txn, d1, 1));
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	RC(mdb_put(txn, d1, &k, &d, 0), EINVAL);
	OK(mdb_dbi_open(txn, "d1", 0, &d1));
	OK(mdb_get(txn, d1, &k, &d));
	CHECK(veq(d, "1"));
	mdb_txn_abort(txn);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

static void test_nested(void)
{
	MDB_env *env = new_env("nested", 1UL << 24, 8, 0), *small;
	MDB_txn *parent, *child, *grand, *t, *r;
	MDB_dbi db, created;
	MDB_cursor *pc, *cc;
	MDB_val k, d;
	char key[32];
	int i, rc;

	OK(mdb_txn_begin(env, NULL, 0, &parent));
	OK(mdb_dbi_open(parent, "db", MDB_CREATE, &db));
	k = sv("a");
	d = sv("1");
	OK(mdb_put(parent, db, &k, &d, 0));
	RC(mdb_txn_begin(env, parent, MDB_RDONLY, &child), MDB_BAD_TXN);

	/* a child sees the parent's changes; while it is active the parent
	 * is blocked */
	OK(mdb_txn_begin(env, parent, 0, &child));
	CHECK(mdb_txn_id(child) == mdb_txn_id(parent));
	CHECK(mdb_txn_env(child) == env);
	OK(mdb_get(child, db, &k, &d));
	CHECK(veq(d, "1"));
	k = sv("b");
	d = sv("2");
	OK(mdb_put(child, db, &k, &d, 0));
	RC(mdb_put(parent, db, &k, &d, 0), MDB_BAD_TXN);
	RC(mdb_get(parent, db, &k, &d), MDB_BAD_TXN);
	RC(mdb_del(parent, db, &k, NULL), MDB_BAD_TXN);
	RC(mdb_cursor_open(parent, db, &pc), MDB_BAD_TXN);
	RC(mdb_txn_begin(env, parent, 0, &t), MDB_BAD_TXN);
	RC(mdb_drop(parent, db, 0), MDB_BAD_TXN);
	{
		/* C still answers this one for a blocked parent */
		unsigned flags;
		DIFFERS(mdb_dbi_flags(parent, db, &flags), 0, MDB_BAD_TXN);
	}
	OK(mdb_txn_commit(child));
	k = sv("b");
	OK(mdb_get(parent, db, &k, &d));
	CHECK(veq(d, "2"));

	/* an aborted child leaves nothing, also not its new databases */
	OK(mdb_txn_begin(env, parent, 0, &child));
	k = sv("c");
	OK(mdb_put(child, db, &k, &d, 0));
	k = sv("a");
	OK(mdb_del(child, db, &k, NULL));
	OK(mdb_dbi_open(child, "created", MDB_CREATE, &created));
	OK(mdb_put(child, created, &k, &d, 0));
	mdb_txn_abort(child);
	k = sv("c");
	RC(mdb_get(parent, db, &k, &d), MDB_NOTFOUND);
	k = sv("a");
	OK(mdb_get(parent, db, &k, &d));
	RC(mdb_put(parent, created, &k, &d, 0), EINVAL);
	RC(mdb_dbi_open(parent, "created", 0, &created), MDB_NOTFOUND);

	/* two levels: the grandchild commits into the child, which aborts */
	OK(mdb_txn_begin(env, parent, 0, &child));
	OK(mdb_txn_begin(env, child, 0, &grand));
	k = sv("g");
	d = sv("grand");
	OK(mdb_put(grand, db, &k, &d, 0));
	OK(mdb_dbi_open(grand, "created", MDB_CREATE, &created));
	OK(mdb_put(grand, created, &k, &d, 0));
	OK(mdb_txn_commit(grand));
	OK(mdb_get(child, db, &k, &d));
	CHECK(veq(d, "grand"));
	OK(mdb_get(child, created, &k, &d));
	mdb_txn_abort(child);
	RC(mdb_get(parent, db, &k, &d), MDB_NOTFOUND);

	/* the parent's cursors are moved into a child for its lifetime
	 * (mdb_cursor_shadow): usable there, restored if it aborts */
	OK(mdb_cursor_open(parent, db, &pc));
	k = sv("b");
	OK(mdb_cursor_get(pc, &k, &d, MDB_SET));
	OK(mdb_txn_begin(env, parent, 0, &child));
	CHECK(mdb_cursor_txn(pc) == child);
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "b") && veq(d, "2"));
	mdb_cursor_close(pc);	/* ignored while the child is active (C) */
	OK(mdb_cursor_get(pc, &k, &d, MDB_FIRST));
	CHECK(veq(k, "a"));
	k = sv("a0");
	d = sv("put through the parent's cursor");
	OK(mdb_cursor_put(pc, &k, &d, 0));
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "a0"));
	OK(mdb_get(child, db, &k, &d));
	CHECK(veq(d, "put through the parent's cursor"));
	/* a grandchild gets it too, and gives it back */
	OK(mdb_txn_begin(env, child, 0, &grand));
	CHECK(mdb_cursor_txn(pc) == grand);
	OK(mdb_cursor_get(pc, &k, &d, MDB_NEXT));
	CHECK(veq(k, "b"));
	OK(mdb_cursor_del(pc, 0));
	mdb_txn_abort(grand);
	CHECK(mdb_cursor_txn(pc) == child);
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "a0"));
	mdb_txn_abort(child);
	CHECK(mdb_cursor_txn(pc) == parent);
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "b") && veq(d, "2"));
	k = sv("a0");
	RC(mdb_get(parent, db, &k, &d), MDB_NOTFOUND);

	/* kept in place across a child's changes, and where the child left
	 * it when the child commits */
	OK(mdb_txn_begin(env, parent, 0, &child));
	OK(mdb_cursor_open(child, db, &cc));
	for (i = 0; i < 2000; i++) {
		snprintf(key, sizeof(key), "a%05d-%s", i, "padding-to-split-pages");
		k = sv(key);
		d = sv(key);
		OK(mdb_cursor_put(cc, &k, &d, 0));
	}
	k = sv("a");
	OK(mdb_del(child, db, &k, NULL));
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "b") && veq(d, "2"));
	OK(mdb_cursor_get(pc, &k, &d, MDB_PREV));
	T("parent cursor in the child: MDB_PREV gives %.*s", (int)k.mv_size, (char *)k.mv_data);
	OK(mdb_cursor_del(pc, 0));
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	T("parent cursor in the child after its delete: %.*s", (int)k.mv_size, (char *)k.mv_data);
	OK(mdb_cursor_get(pc, &k, &d, MDB_PREV));
	T("parent cursor in the child: MDB_PREV gives %.*s", (int)k.mv_size, (char *)k.mv_data);
	OK(mdb_txn_commit(child));	/* closes cc */
	CHECK(mdb_cursor_txn(pc) == parent);
	OK(mdb_cursor_get(pc, &k, &d, MDB_GET_CURRENT));
	T("parent cursor after the child: %.*s", (int)k.mv_size, (char *)k.mv_data);
	OK(mdb_cursor_get(pc, &k, &d, MDB_NEXT));
	CHECK(veq(k, "b") && veq(d, "2"));
	RC(mdb_cursor_get(pc, &k, &d, MDB_NEXT), MDB_NOTFOUND);
	trace_stat("db after the child's 2000 puts", parent, db);

	/* committing the parent commits its active child first */
	OK(mdb_txn_begin(env, parent, 0, &child));
	k = sv("e");
	d = sv("from the child");
	OK(mdb_put(child, db, &k, &d, 0));
	OK(mdb_txn_commit(parent));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_get(r, db, &k, &d));
	CHECK(veq(d, "from the child"));
	trace_stat("db committed", r, db);
	mdb_txn_abort(r);

	/* aborting the parent aborts its child */
	OK(mdb_txn_begin(env, NULL, 0, &parent));
	OK(mdb_txn_begin(env, parent, 0, &child));
	k = sv("f");
	OK(mdb_put(child, db, &k, &d, 0));
	mdb_txn_abort(parent);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	RC(mdb_get(r, db, &k, &d), MDB_NOTFOUND);
	mdb_txn_abort(r);
	mdb_env_close(env);

	/* a child that fails makes its parent fail */
	small = new_env("nested-full", 1UL << 18, 4, 0);
	OK(mdb_txn_begin(small, NULL, 0, &parent));
	OK(mdb_dbi_open(parent, NULL, 0, &db));
	OK(mdb_txn_begin(small, parent, 0, &child));
	{
		char big[3000];
		memset(big, 'x', sizeof(big));
		for (i = 0; i < 100000; i++) {
			snprintf(key, sizeof(key), "%08d", i);
			k = sv(key);
			d = bv(big, sizeof(big));
			if ((rc = mdb_put(child, db, &k, &d, 0)) != 0)
				break;
		}
	}
	RC(rc, MDB_MAP_FULL);
	RC(mdb_txn_commit(child), MDB_BAD_TXN);
	RC(mdb_put(parent, db, &k, &d, 0), MDB_BAD_TXN);
	RC(mdb_txn_commit(parent), MDB_BAD_TXN);
	mdb_env_close(small);
}

/* ---------------------------------------------------------------------- */

/* A trace of every database of the environment at <scratch>/<name>. */
static void trace_env(const char *name, const char *what)
{
	MDB_env *env;
	MDB_txn *r;
	MDB_cursor *c;
	MDB_envinfo info;
	MDB_val k, d;
	MDB_dbi dbi;
	char path[1200], label[200], dbname[512];
	unsigned flags;

	snprintf(path, sizeof(path), "%s/%s", scratch, name);
	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 16));
	OK(mdb_env_open(env, path, MDB_RDONLY | MDB_NOTLS, 0664));
	OK(mdb_env_info(env, &info));
	T("%s: last txnid %zu, last page %zu", what, info.me_last_txnid, info.me_last_pgno);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	snprintf(label, sizeof(label), "%s main", what);
	trace_stat(label, r, MAIN_DBI);
	OK(mdb_cursor_open(r, MAIN_DBI, &c));
	while (mdb_cursor_get(c, &k, &d, MDB_NEXT) == 0) {
		snprintf(dbname, sizeof(dbname), "%.*s", (int)k.mv_size, (char *)k.mv_data);
		if (mdb_dbi_open(r, dbname, 0, &dbi) != 0)
			continue;
		OK(mdb_dbi_flags(r, dbi, &flags));
		snprintf(label, sizeof(label), "%s %s (flags %#x)", what, dbname, flags);
		if (flags & MDB_DUPSORT)
			trace_stat(label, r, dbi);
		else
			trace_all(label, r, dbi);
	}
	mdb_cursor_close(c);
	mdb_txn_abort(r);
	mdb_env_close(env);
}

static void test_copy(const char *name)
{
	MDB_env *env;
	char path[1200], dst[1200];
	int fd;

	snprintf(path, sizeof(path), "%s/%s", scratch, name);
	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 16));
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));

	snprintf(dst, sizeof(dst), "%s/%s-copy", scratch, name);
	mkdir(dst, 0775);
	OK(mdb_env_copy(env, dst));
	RC(mdb_env_copy(env, dst), COMPAT_EEXIST);
	snprintf(dst, sizeof(dst), "%s/%s-compact", scratch, name);
	mkdir(dst, 0775);
	OK(mdb_env_copy2(env, dst, MDB_CP_COMPACT));
	snprintf(dst, sizeof(dst), "%s/%s-fd", scratch, name);
	mkdir(dst, 0775);
	snprintf(dst, sizeof(dst), "%s/%s-fd/data.mdb", scratch, name);
	fd = open(dst, O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, COMPAT_CREATE_MODE);
	CHECK(fd >= 0);
	OK(mdb_env_copyfd(env, COMPAT_FILEHANDLE(fd)));
	close(fd);
	snprintf(dst, sizeof(dst), "%s/%s-fd-compact", scratch, name);
	mkdir(dst, 0775);
	snprintf(dst, sizeof(dst), "%s/%s-fd-compact/data.mdb", scratch, name);
	fd = open(dst, O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, COMPAT_CREATE_MODE);
	CHECK(fd >= 0);
	OK(mdb_env_copyfd2(env, COMPAT_FILEHANDLE(fd), MDB_CP_COMPACT));
	close(fd);
	mdb_env_close(env);

	trace_env(name, "source");
	snprintf(dst, sizeof(dst), "%s-copy", name);
	trace_env(dst, "plain copy");
	snprintf(dst, sizeof(dst), "%s-compact", name);
	trace_env(dst, "compact copy");
	snprintf(dst, sizeof(dst), "%s-fd", name);
	trace_env(dst, "plain copy to an fd");
	snprintf(dst, sizeof(dst), "%s-fd-compact", name);
	trace_env(dst, "compact copy to an fd");
}

/* ---------------------------------------------------------------------- */

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: capi_write <scratch-dir>\n");
		return 2;
	}
	snprintf(scratch, sizeof(scratch), "%s", argv[1]);
	setvbuf(stdout, NULL, _IOLBF, 0);
	test_txns();
	test_dbi();
	test_put_get_del();
	test_cursors();
	test_comparators();
	test_comparator_reuse();
	test_errors();
	test_threads();
	test_drop();
	test_nested();
	test_copy("cursors");
	test_copy("put");
	test_dupfixed_small();
	test_copy("small");

	printf("capi_write: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
