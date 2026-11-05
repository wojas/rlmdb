/* Exercises the write side of the LMDB C API and its error codes.
 *
 *   capi_write <scratch-dir>
 *
 * Companion of capi_read.c, built the same way: against the C library (the
 * oracle: every expectation below must hold for C LMDB 0.9.37) and against
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

/* A documented difference (DETAILS.md): C returns c_rc, Rust rust_rc. */
#ifdef LMDB_RUST
#define DIFFERS(expr, c_rc, rust_rc) RC(expr, rust_rc)
#else
#define DIFFERS(expr, c_rc, rust_rc) RC(expr, c_rc)
#endif

#ifdef LMDB_RUST
#define GAP(name, cond) do { if (cond) { failures++; \
	fprintf(stderr, "%s:%d: known gap \"%s\" now behaves like C: " \
		"update the test and rlmdb-capi/DETAILS.md\n", __FILE__, __LINE__, name); \
	} else printf("known gap: %s\n", name); } while (0)
#define DEVIATION(stmt) do { stmt; } while (0)
#else
#define GAP(name, cond) CHECK(cond)
#define DEVIATION(stmt) do { checks++; } while (0)
#endif

/* The two core databases (mdb.c). */
#define FREE_DBI 0
#define MAIN_DBI 1

/* What mdb_env_copyfd returns for COMPAT_NO_FILEHANDLE: the error of C's
 * first write to it. */
#ifdef _WIN32
#define NO_FILE_ERROR 6	/* ERROR_INVALID_HANDLE, from WriteFile */
#else
#define NO_FILE_ERROR EBADF
#endif

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
	/* MDB_RESERVE is not for DUPSORT (C does not check, see
	 * KNOWN_C_BUGS.md row 16) */
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
	/* so much so that it is not read: neither its NULL nor its unmapped
	 * address matters (C drops it after checking the handle) */
	k = sv("del-ignores-data");
	d = sv("v");
	OK(mdb_put(txn, db, &k, &d, 0));
	d = bv(NULL, 1);
	OK(mdb_del(txn, db, &k, &d));
	d = sv("v");
	OK(mdb_put(txn, db, &k, &d, 0));
	d = bv((void *)1, 1);
	RC(mdb_del(txn, 99, &k, &d), EINVAL);
	OK(mdb_del(txn, db, &k, &d));
	RC(mdb_del(txn, db, &k, &d), MDB_NOTFOUND);
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

	/* MDB_DUPFIXED with mismatched sizes: refused (KNOWN_C_BUGS.md row 12) */
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
	 * rlmdb deletes and inserts: KNOWN_C_BUGS.md row 38.) */
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

/* mdb_cursor_count of a cursor on a key after another cursor (or mdb_put)
 * put duplicates of that key: converting a single item, into a sub-page,
 * growing it, promoting it to a sub-DB, into a sub-DB, splitting its leaf;
 * before and after the cursor's item. C's count is the one before the put
 * (its cursor keeps a stale copy of the duplicates' record,
 * KNOWN_C_BUGS.md row 14): one short where the node was rewritten, short by
 * every added item in a sub-DB. rlmdb's is right. The positions are C's
 * (traced). */

static MDB_val be32(unsigned char *b, unsigned v)
{
	b[0] = (unsigned char)(v >> 24);
	b[1] = (unsigned char)(v >> 16);
	b[2] = (unsigned char)(v >> 8);
	b[3] = (unsigned char)v;
	return bv(b, 4);
}

static unsigned be32_of(MDB_val d)
{
	const unsigned char *b = d.mv_data;
	return d.mv_size == 4 ? (unsigned)b[0] << 24 | (unsigned)b[1] << 16 |
		(unsigned)b[2] << 8 | b[3] : 0;
}

static size_t leaf_pages(MDB_txn *txn, MDB_dbi dbi)
{
	MDB_stat st;
	st.ms_leaf_pages = 0;
	OK(mdb_stat(txn, dbi, &st));
	return st.ms_leaf_pages;
}

/* A new database of `flags` named `name` with keys "a" and "z" around "k".
 * Puts items 10, 20, ... under "k" until n are there, or (n == 0) until a
 * put adds a leaf page for the (from-1)-th time (the first time: "k" moves
 * to a sub-DB; then its leaf splits); returns that put's number. */
static unsigned fill_k(MDB_txn *txn, const char *name, unsigned flags, unsigned n,
	int from, MDB_dbi *dbp)
{
	unsigned char b[4];
	MDB_val k = sv("k"), d;
	unsigned i;
	int added = 0;

	OK(mdb_dbi_open(txn, name, MDB_CREATE | flags, dbp));
	d = be32(b, 1);
	k = sv("a");
	OK(mdb_put(txn, *dbp, &k, &d, 0));
	k = sv("z");
	OK(mdb_put(txn, *dbp, &k, &d, 0));
	k = sv("k");
	for (i = 1; n ? i <= n : i < 1000000; i++) {
		size_t lp = leaf_pages(txn, *dbp);
		d = be32(b, 10 * i);
		OK(mdb_put(txn, *dbp, &k, &d, 0));
		if (!n && leaf_pages(txn, *dbp) != lp && ++added == from)
			return i;
	}
	return n;
}

/* Cursor A on item `on` of "k" (items 10 ... 10n), then `how`: 0 another
 * cursor puts `v`, 1 mdb_put puts it, 2 MDB_MULTIPLE puts v, v+1, v+2. */
static void count_case(MDB_txn *txn, const char *what, unsigned flags, int how,
	unsigned n, unsigned on, unsigned v)
{
	static int seq;
	unsigned char b[4], items[12];
	char name[32];
	MDB_dbi db;
	MDB_cursor *a, *c;
	MDB_val k, d, m[2];
	size_t cnt, truth;
	unsigned added = how == 2 ? 3 : 1, cur, next, prev;
	int rc, rc_next, rc_prev;

	snprintf(name, sizeof(name), "count%d", seq++);
	fill_k(txn, name, flags, n, 0, &db);
	OK(mdb_cursor_open(txn, db, &a));
	OK(mdb_cursor_open(txn, db, &c));
	k = sv("k");
	d = be32(b, on);
	OK(mdb_cursor_get(a, &k, &d, MDB_GET_BOTH));
	d = be32(b, v);
	if (how == 0) {
		OK(mdb_cursor_put(c, &k, &d, 0));
	} else if (how == 1) {
		OK(mdb_put(txn, db, &k, &d, 0));
	} else {
		be32(items, v);
		be32(items + 4, v + 1);
		be32(items + 8, v + 2);
		m[0] = bv(items, 4);
		m[1].mv_size = 3;
		m[1].mv_data = NULL;
		OK(mdb_cursor_put(c, &k, m, MDB_MULTIPLE));
	}
	OK(mdb_cursor_get(c, &k, &d, MDB_SET));
	OK(mdb_cursor_count(c, &truth));
	CHECK(truth == n + added);
	OK(mdb_cursor_count(a, &cnt));
#ifdef LMDB_RUST
	CHECK(cnt == truth);
#else
	CHECK(cnt < truth);	/* stale, KNOWN_C_BUGS.md row 14 */
#endif
	rc = mdb_cursor_get(a, &k, &d, MDB_GET_CURRENT);
	cur = rc ? 0 : be32_of(d);
	rc_next = mdb_cursor_get(a, &k, &d, MDB_NEXT_DUP);
	next = rc_next ? 0 : be32_of(d);
	rc_prev = mdb_cursor_get(a, &k, &d, MDB_PREV_DUP);
	prev = rc_prev ? 0 : be32_of(d);
	T("count after a put, %s (flags %#x, %s): %zu items, A on %u (%d), next dup %u (%d), "
		"then previous %u (%d)", what, flags, how == 0 ? "cursor" : how == 1 ? "mdb_put" :
		"MDB_MULTIPLE", truth, cur, rc, next, rc_next, prev, rc_prev);
	mdb_cursor_close(a);
	mdb_cursor_close(c);
}

static void test_count_after_other_put(void)
{
	MDB_env *env = new_env("count", 1UL << 24, 200, 0);
	unsigned flags[2] = { MDB_DUPSORT | MDB_DUPFIXED, MDB_DUPSORT };
	MDB_txn *txn;
	MDB_dbi probe;
	unsigned s, t;
	char name[32];
	int f, how;

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (f = 0; f < 2; f++) {
		snprintf(name, sizeof(name), "probe%d", f);
		/* "k" moves to a sub-DB with item s, its leaf splits with item t */
		s = fill_k(txn, name, flags[f], 0, 1, &probe);
		snprintf(name, sizeof(name), "probe-split%d", f);
		t = fill_k(txn, name, flags[f], 0, 2, &probe);
		CHECK(s > 4 && t > s);
		for (how = 0; how < (f == 0 ? 3 : 2); how++) {
			count_case(txn, "single item, after", flags[f], how, 1, 10, 15);
			count_case(txn, "single item, before", flags[f], how, 1, 10, 5);
			count_case(txn, "sub-page, after", flags[f], how, 3, 20, 25);
			count_case(txn, "sub-page, before", flags[f], how, 3, 20, 15);
			count_case(txn, "sub-page grows", flags[f], how, 4, 20, 45);
			count_case(txn, "to a sub-DB, after", flags[f], how, s - 1, 20, 10 * s + 5);
			count_case(txn, "to a sub-DB, before", flags[f], how, s - 1, 20, 5);
			count_case(txn, "sub-DB, after", flags[f], how, s + 5, 20, 25);
			count_case(txn, "sub-DB, before", flags[f], how, s + 5, 20, 15);
			count_case(txn, "sub-DB leaf splits, after", flags[f], how, t - 1, 10, 10 * t + 5);
			count_case(txn, "sub-DB leaf splits, before", flags[f], how, t - 1,
				10 * (t - 1), 5);
		}
	}
	OK(mdb_txn_commit(txn));
	mdb_env_close(env);
}

/* ---------------------------------------------------------------------- */

/* Moves after a failed exact search (KNOWN_C_BUGS.md row 59). Cursor A's
 * MDB_SET of "b" leaves it on the node of "k" (between "a" and "z") with
 * its sub-cursor reset; MDB_NEXT or MDB_PREV then enters the duplicates of
 * "k". C enters them with what the sub-cursor last held: a record from
 * before another cursor added items to "k" (the count is short; after the
 * sub-DB's root split the walk sees only the old root; where the put moved
 * a sub-page C reads the main leaf as its root and crashes), another key's
 * record ("m", whose items C walks under the key "k"), none (a new cursor,
 * row 6: C asserts), or a C_DEL mark left by a delete ("b" deleted under
 * the cursor, row 29: C's MDB_NEXT returns the first item twice). rlmdb
 * sets the sub-cursor up from the node. The cases C survives run against
 * both libraries (C's results written in the checks), the others against
 * rlmdb only. */

enum { ON_K, ON_M, FRESH, DELETED };

/* What miss_case() saw. */
struct miss {
	unsigned total, min, max;	/* items of "k" in the end, the smallest,
					 * the largest */
	int rc_count, rc_cur;	/* count and MDB_GET_CURRENT after the search
				 * (not for DELETED: 0) */
	int rc;			/* the move */
	size_t count;		/* count after it */
	unsigned first;		/* the item it returned */
	unsigned seen, sorted;	/* items of "k" the walk saw, how many in order */
	unsigned fourbyte;	/* how many had 4 bytes */
	char end;		/* the key the walk ended on, '-' for none */
};

#define TRY(expr) do { int rc_ = (expr); if (rc_) return rc_; } while (0)

/* A new database with "a" and "z" (item 1) and "k" (items 10 ... 10n),
 * "m" with nm items of 8 bytes (ON_M) or "b" (DELETED); A positioned as
 * `before` says, then the failed MDB_SET; another cursor adds the items 5,
 * 15, ... (`add` of them, with MDB_MULTIPLE if `multiple`); then `op` and
 * the same move while on "k". No checks of its own (the cases C crashes
 * on run in DEVIATION()): 0, or the error of a step before the move. */
static int miss_case(MDB_txn *txn, unsigned flags, unsigned n, int before, unsigned nm,
	unsigned add, int multiple, MDB_cursor_op op, struct miss *r)
{
	static int seq;
	unsigned char b[4], b8[8], *items;
	char name[32];
	MDB_dbi db;
	MDB_cursor *a, *c;
	MDB_val k, d, m[2];
	unsigned i, v, prev = 0;
	int rc;

	memset(r, 0, sizeof(*r));
	r->total = n + add;
	r->min = add ? 5 : 10;
	r->max = add && 10 * (add - 1) + 5 > 10 * n ? 10 * (add - 1) + 5 : 10 * n;
	snprintf(name, sizeof(name), "miss%d", seq++);
	TRY(mdb_dbi_open(txn, name, MDB_CREATE | flags, &db));
	d = be32(b, 1);
	k = sv("a");
	TRY(mdb_put(txn, db, &k, &d, 0));
	k = sv("z");
	TRY(mdb_put(txn, db, &k, &d, 0));
	if (before == DELETED) {
		k = sv("b");
		TRY(mdb_put(txn, db, &k, &d, 0));
	}
	k = sv("k");
	for (i = 1; i <= n; i++) {
		d = be32(b, 10 * i);
		TRY(mdb_put(txn, db, &k, &d, 0));
	}
	k = sv("m");
	for (i = 0; before == ON_M && i < nm; i++) {
		memset(b8, 0, 4);
		be32(b8 + 4, i);
		d = bv(b8, 8);
		TRY(mdb_put(txn, db, &k, &d, 0));
	}
	TRY(mdb_cursor_open(txn, db, &a));
	TRY(mdb_cursor_open(txn, db, &c));
	if (before != FRESH) {
		k = sv(before == ON_K ? "k" : before == ON_M ? "m" : "b");
		TRY(mdb_cursor_get(a, &k, &d, MDB_SET));
	}
	if (before == DELETED) {
		k = sv("b");
		TRY(mdb_del(txn, db, &k, NULL));
	}
	k = sv("b");
	rc = mdb_cursor_get(a, &k, &d, MDB_SET);
	if (rc != MDB_NOTFOUND)
		return rc ? rc : -1;
	r->rc_count = mdb_cursor_count(a, &r->count);
	/* C's MDB_GET_CURRENT through the sub-cursor clears its C_DEL, also
	 * when it fails. */
	if (before != DELETED)
		r->rc_cur = mdb_cursor_get(a, &k, &d, MDB_GET_CURRENT);
	k = sv("k");
	if (multiple) {
		items = malloc(4 * add);
		for (i = 0; i < add; i++)
			be32(items + 4 * i, 10 * i + 5);
		m[0] = bv(items, 4);
		m[1].mv_size = add;
		m[1].mv_data = NULL;
		rc = mdb_cursor_put(c, &k, m, MDB_MULTIPLE);
		free(items);
		TRY(rc);
	}
	for (i = 0; !multiple && i < add; i++) {
		d = be32(b, 10 * i + 5);
		TRY(mdb_cursor_put(c, &k, &d, 0));
	}
	r->count = 0;
	rc = r->rc = mdb_cursor_get(a, &k, &d, op);
	if (!rc) {
		r->first = be32_of(d);
		if (mdb_cursor_count(a, &r->count))
			r->count = 0;
	}
	while (!rc && veq(k, "k")) {
		v = be32_of(d);
		r->fourbyte += d.mv_size == 4;
		r->sorted += !r->seen || (op == MDB_NEXT ? v > prev : v < prev);
		r->seen++;
		prev = v;
		rc = mdb_cursor_get(a, &k, &d, op);
	}
	r->end = rc ? '-' : *(char *)k.mv_data;
	mdb_cursor_close(a);
	mdb_cursor_close(c);
	return 0;
}

/* rlmdb's result: every item of "k" once, in order, from the first (the
 * last for MDB_PREV), and the right count. */
static int miss_right(const struct miss *r, MDB_cursor_op op)
{
	return r->first == (op == MDB_NEXT ? r->min : r->max) && r->count == r->total &&
		r->seen == r->total && r->sorted == r->total && r->fourbyte == r->total;
}

/* How C's walk differs from rlmdb's, in the cases it survives. */
enum { C_RIGHT, C_COUNT, C_OLD_ROOT, C_OTHER_KEY, C_TWICE };

/* A case run against both libraries. */
static void miss_both(MDB_txn *txn, const char *what, unsigned flags, unsigned n, int before,
	unsigned nm, unsigned add, int multiple, MDB_cursor_op op, int c_result)
{
	struct miss r;

	RC(miss_case(txn, flags, n, before, nm, add, multiple, op, &r), 0);
	/* C's argument checks: the sub-cursor is not on an item. */
	RC(r.rc_count, EINVAL);
	RC(r.rc_cur, before == DELETED ? 0 : EINVAL);
	RC(r.rc, 0);
	CHECK(r.end == (op == MDB_PREV ? 'a' : before == ON_M ? 'm' : 'z'));
#ifdef LMDB_RUST
	CHECK(miss_right(&r, op));
#else
	switch (c_result) {
	case C_RIGHT:
		CHECK(miss_right(&r, op));
		break;
	case C_COUNT:	/* the count before the put, as row 14 */
		CHECK(r.count == n && r.seen == r.total && r.sorted == r.total);
		break;
	case C_OLD_ROOT:	/* the old root's items only */
		CHECK(r.count == n && r.seen < r.total && r.sorted == r.seen);
		break;
	case C_OTHER_KEY:	/* the items of "m" */
		CHECK(r.count == nm && r.seen == nm && r.fourbyte == 0);
		break;
	default:	/* C_TWICE: the first item twice */
		CHECK(r.first == 10 && r.count == n && r.seen == n + 1 && r.sorted == n);
		break;
	}
#endif
	T("move after a failed search, %s (flags %#x, %s): count %d, current %d, move %d, "
		"ends on %c", what, flags, op == MDB_NEXT ? "MDB_NEXT" : "MDB_PREV", r.rc_count,
		r.rc_cur, r.rc, r.end);
}

#ifdef LMDB_RUST
/* A case C crashes on, asserts in or answers with garbage, against rlmdb:
 * 0, or the step that is wrong. No checks of its own, as DEVIATION()
 * counts one. */
static int miss_rust(MDB_txn *txn, unsigned flags, unsigned n, int before, unsigned add,
	int multiple, MDB_cursor_op op)
{
	struct miss r;
	int rc = miss_case(txn, flags, n, before, 0, add, multiple, op, &r);

	if (rc)
		return rc;
	if (r.rc_count != EINVAL || r.rc_cur != EINVAL)
		return 1;
	if (r.rc)
		return 2;
	if (r.end != (op == MDB_PREV ? 'a' : 'z'))
		return 3;
	return miss_right(&r, op) ? 0 : 4;
}
#endif

static void test_moves_after_failed_search(void)
{
	MDB_env *env = new_env("miss", 1UL << 24, 200, 0);
	unsigned flags[2] = { MDB_DUPSORT | MDB_DUPFIXED, MDB_DUPSORT };
	MDB_cursor_op ops[2] = { MDB_NEXT, MDB_PREV };
	MDB_txn *txn;
	MDB_dbi probe;
	unsigned s, t, f, o;
	char name[32];

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (f = 0; f < 2; f++) {
		snprintf(name, sizeof(name), "probe%u", f);
		/* "k" moves to a sub-DB with item s, its leaf splits with item t */
		s = fill_k(txn, name, flags[f], 0, 1, &probe);
		snprintf(name, sizeof(name), "probe-split%u", f);
		t = fill_k(txn, name, flags[f], 0, 2, &probe);
		CHECK(s > 4 && t > s + 1);
		for (o = 0; o < 2; o++) {
			MDB_cursor_op op = ops[o];
			int fixed = f == 0;

			miss_both(txn, "sub-DB, another cursor adds one", flags[f], s + 1, ON_K, 0,
				1, 0, op, C_COUNT);
			miss_both(txn, "sub-DB's root splits", flags[f], s + 1, ON_K, 0, t - s + 1,
				0, op, C_OLD_ROOT);
			miss_both(txn, "another key's sub-page", flags[f], 3, ON_M, 2, 0, 0, op,
				C_OTHER_KEY);
			miss_both(txn, "another key's sub-DB", flags[f], 3, ON_M, s + 1, 0, 0, op,
				C_OTHER_KEY);
			miss_both(txn, "a sub-DB, another key's sub-page", flags[f], s + 1, ON_M, 2,
				0, 0, op, C_OTHER_KEY);
			miss_both(txn, "sub-page, a delete marked it", flags[f], 3, DELETED, 0, 0, 0,
				op, op == MDB_NEXT ? C_TWICE : C_RIGHT);
			miss_both(txn, "sub-DB, a delete marked it", flags[f], s + 1, DELETED, 0, 0,
				0, op, op == MDB_NEXT ? C_TWICE : C_RIGHT);
			miss_both(txn, "sub-page, the record is right", flags[f], 3, ON_K, 0, 0, 0,
				op, C_RIGHT);
			miss_both(txn, "sub-DB, the record is right", flags[f], s + 1, ON_K, 0, 0, 0,
				op, C_RIGHT);
			if (fixed) {
				/* The sub-page has room for the item: it does not move. */
				miss_both(txn, "sub-page, another cursor adds one", flags[f], 3, ON_K,
					0, 1, 0, op, C_COUNT);
				miss_both(txn, "sub-page, MDB_MULTIPLE of one", flags[f], 3, ON_K, 0,
					1, 1, op, C_COUNT);
				miss_both(txn, "sub-DB's root splits, MDB_MULTIPLE", flags[f], s + 1,
					ON_K, 0, t - s + 1, 1, op, C_OLD_ROOT);
				/* The sub-page moves: C crashes. */
				DEVIATION(RC(miss_rust(txn, flags[f], 3, ON_K, 2, 1, op), 0));
			} else {
				DEVIATION(RC(miss_rust(txn, flags[f], 3, ON_K, 1, 0, op), 0));
			}
			/* The sub-page becomes a sub-DB: C reads garbage. */
			DEVIATION(RC(miss_rust(txn, flags[f], s - 1, ON_K, 2, 0, op), 0));
			/* Never on duplicates (row 6): C asserts. */
			DEVIATION(RC(miss_rust(txn, flags[f], 3, FRESH, 0, 0, op), 0));
			DEVIATION(RC(miss_rust(txn, flags[f], s + 1, FRESH, 0, 0, op), 0));
		}
	}
	OK(mdb_txn_commit(txn));
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

/* mdb_set_compare and mdb_set_dupsort after cursors were opened on the
 * handles, in a read and in a write transaction of a freshly opened
 * environment (so the handles start with the default orders). C's cursor
 * finds the key order and the order of a key's single data item through
 * the handle and uses the new ones at once; the order of a key's
 * duplicates is copied into the cursor when it is opened
 * (mdb_xcursor_init0) and stays. */
static void test_comparator_after_cursor_open(void)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi keys, dups;
	MDB_cursor *c, *dc;
	MDB_val k, d;
	char x[2] = "a";
	int rdonly, rc;

	env = new_env("cmplate", 1UL << 22, 4, 0);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "keys", MDB_CREATE, &keys));
	OK(mdb_set_compare(txn, keys, revcmp));
	OK(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dups));
	OK(mdb_set_dupsort(txn, dups, revcmp));
	for (x[0] = 'a'; x[0] <= 'e'; x[0]++) {
		k = sv(x);
		OK(mdb_put(txn, keys, &k, &k, 0));
		d = k;
		k = sv("many");
		OK(mdb_put(txn, dups, &k, &d, 0));
	}
	k = sv("one");
	d = sv("c");
	OK(mdb_put(txn, dups, &k, &d, 0));
	OK(mdb_txn_commit(txn));
	mdb_env_close(env);

	for (rdonly = 0; rdonly < 2; rdonly++) {
		env = new_env("cmplate", 1UL << 22, 4, 0);
		OK(mdb_txn_begin(env, NULL, rdonly ? MDB_RDONLY : 0, &txn));
		OK(mdb_dbi_open(txn, "keys", 0, &keys));
		OK(mdb_dbi_open(txn, "dups", 0, &dups));
		OK(mdb_cursor_open(txn, keys, &c));
		OK(mdb_cursor_open(txn, dups, &dc));
		OK(mdb_set_compare(txn, keys, revcmp));
		OK(mdb_set_dupsort(txn, dups, revcmp));
		k = sv("a");
		OK(mdb_get(txn, keys, &k, &d));
		OK(mdb_cursor_get(c, &k, &d, MDB_SET));
		CHECK(veq(d, "a"));
		k = sv("b");
		OK(mdb_cursor_get(c, &k, &d, MDB_SET_KEY));
		OK(mdb_cursor_get(c, &k, &d, MDB_NEXT));
		CHECK(veq(k, "a"));
		k = sv("bb");	/* between "c" and "b" in reverse order */
		OK(mdb_cursor_get(c, &k, &d, MDB_SET_RANGE));
		CHECK(veq(k, "b"));
		/* A key's single item: "a" sorts after "c" now. */
		k = sv("one");
		d = sv("a");
		RC(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH_RANGE), MDB_NOTFOUND);
		d = sv("d");
		OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH_RANGE));
		CHECK(veq(d, "c"));
		/* A key's duplicates: the cursor still searches them in the
		 * default order, which they are not stored in. */
		k = sv("many");
		d = sv("b");
		rc = mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH);
		T("cmplate (%s): duplicate b through the earlier cursor: %d",
			rdonly ? "read" : "write", rc);
		mdb_cursor_close(dc);
		OK(mdb_cursor_open(txn, dups, &dc));
		d = sv("b");
		OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH));
		CHECK(veq(d, "b"));
		mdb_cursor_close(dc);
		mdb_cursor_close(c);
		mdb_txn_abort(txn);
		mdb_env_close(env);
	}
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

#ifdef LMDB_RUST
/* KNOWN_C_BUGS.md row 20: the abort of a child that deleted a database with
 * mdb_drop(del = 1) reopens the handle for the parent, and with it the
 * comparison function installed on it (mdb_set_compare, dup 0, or
 * mdb_set_dupsort, dup 1): mdb_cmp/mdb_dcmp and what the parent reads and
 * writes keep the database's order, also after the commit. (C closes the
 * handle for good.) Returns 0, or the step that failed; it has no checks
 * of its own, as DEVIATION() counts one. */
static int nested_drop_keeps_comparator(int dup)
{
	MDB_env *env;
	MDB_txn *txn, *child;
	MDB_dbi dbi;
	MDB_cursor *c;
	MDB_val k, d, a = sv("a"), b = sv("b");
	int (*cmp)(MDB_txn *, MDB_dbi, const MDB_val *, const MDB_val *) =
		dup ? mdb_dcmp : mdb_cmp;
	char path[1200], x[2] = "a";
	int step = 0;

	snprintf(path, sizeof(path), "%s/dropabort-%d", scratch, dup);
	mkdir(path, 0775);
	if (mdb_env_create(&env) || mdb_env_set_maxdbs(env, 2) ||
		mdb_env_open(env, path, MDB_NOTLS, 0664))
		return 1;
	if (mdb_txn_begin(env, NULL, 0, &txn) ||
		mdb_dbi_open(txn, "db", MDB_CREATE | (dup ? MDB_DUPSORT : 0), &dbi) ||
		(dup ? mdb_set_dupsort : mdb_set_compare)(txn, dbi, revcmp))
		return 1;
	for (x[0] = 'a'; x[0] <= 'e'; x[0]++) {
		k = dup ? sv("k") : sv(x);
		d = sv(x);
		if (mdb_put(txn, dbi, &k, &d, 0))
			return 2;
	}
	if (mdb_txn_commit(txn) || mdb_txn_begin(env, NULL, 0, &txn))
		return 3;
	k = dup ? sv("k") : a;
	if (mdb_get(txn, dbi, &k, &d) || cmp(txn, dbi, &a, &b) <= 0)
		return 4;
	if (mdb_txn_begin(env, txn, 0, &child) || mdb_drop(child, dbi, 1))
		return 5;
	mdb_txn_abort(child);
	if (cmp(txn, dbi, &a, &b) <= 0)
		return 6;
	if (mdb_cursor_open(txn, dbi, &c) || mdb_cursor_get(c, &k, &d, MDB_FIRST) ||
		!veq(dup ? d : k, "e"))
		return 7;
	k = dup ? sv("k") : sv("f");
	d = sv("f");
	if (mdb_put(txn, dbi, &k, &d, 0) || mdb_txn_commit(txn))
		return 8;
	if (mdb_txn_begin(env, NULL, MDB_RDONLY, &txn) || cmp(txn, dbi, &a, &b) <= 0)
		return 9;
	if (mdb_cursor_open(txn, dbi, &c) || mdb_cursor_get(c, &k, &d, MDB_FIRST) ||
		!veq(dup ? d : k, "f"))
		step = 10;
	mdb_cursor_close(c);
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return step;
}
#endif

static void test_comparator_reuse(void)
{
	int dup, nested;

	for (dup = 0; dup < 2; dup++)
		for (nested = 0; nested < 2; nested++)
			comparator_reuse(dup, nested);
	DEVIATION(RC(nested_drop_keeps_comparator(0), 0));
	DEVIATION(RC(nested_drop_keeps_comparator(1), 0));
}

/* ---------------------------------------------------------------------- */

static void test_errors(void)
{
	MDB_env *env = new_env("full", 1UL << 18, 4, 0);
	MDB_txn *txn;
	MDB_dbi db;
	MDB_cursor *c;
	MDB_val k, d, bad;
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
	bad = bv((void *)1, 1);	/* ignored without DUPSORT, never read */
	RC(mdb_del(txn, db, &k, &bad), MDB_BAD_TXN);
	RC(mdb_cursor_get(c, &k, &d, MDB_FIRST), MDB_BAD_TXN);
	/* before the operation and its arguments are looked at */
	RC(mdb_cursor_get(c, &k, &d, (MDB_cursor_op)999), MDB_BAD_TXN);
	RC(mdb_cursor_get(c, &k, NULL, MDB_GET_BOTH), MDB_BAD_TXN);
	RC(mdb_cursor_get(c, NULL, &d, MDB_SET), MDB_BAD_TXN);
	RC(mdb_cursor_get(c, &k, NULL, MDB_GET_MULTIPLE), MDB_BAD_TXN);
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
	MDB_env *env = new_env("nested", 1UL << 24, 8, 0), *small_env;
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

	/* at any depth: the child commits its active grandchild first */
	OK(mdb_txn_begin(env, NULL, 0, &parent));
	OK(mdb_txn_begin(env, parent, 0, &child));
	OK(mdb_txn_begin(env, child, 0, &grand));
	k = sv("e2");
	d = sv("from the grandchild");
	OK(mdb_put(grand, db, &k, &d, 0));
	OK(mdb_txn_commit(parent));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_get(r, db, &k, &d));
	CHECK(veq(d, "from the grandchild"));
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
	small_env = new_env("nested-full", 1UL << 18, 4, 0);
	OK(mdb_txn_begin(small_env, NULL, 0, &parent));
	OK(mdb_dbi_open(parent, NULL, 0, &db));
	OK(mdb_txn_begin(small_env, parent, 0, &child));
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

	/* also when it is still active, deeper down: committing the parent
	 * commits it first, which fails, and everything is aborted */
	OK(mdb_txn_begin(small_env, NULL, 0, &parent));
	k = sv("parent");
	d = sv("lost");
	OK(mdb_put(parent, db, &k, &d, 0));
	OK(mdb_txn_begin(small_env, parent, 0, &child));
	OK(mdb_txn_begin(small_env, child, 0, &grand));
	{
		char big[3000];
		memset(big, 'y', sizeof(big));
		for (i = 0; i < 100000; i++) {
			snprintf(key, sizeof(key), "%08d", i);
			k = sv(key);
			d = bv(big, sizeof(big));
			if ((rc = mdb_put(grand, db, &k, &d, 0)) != 0)
				break;
		}
	}
	RC(rc, MDB_MAP_FULL);
	RC(mdb_txn_commit(parent), MDB_BAD_TXN);
	OK(mdb_txn_begin(small_env, NULL, MDB_RDONLY, &r));
	k = sv("parent");
	RC(mdb_get(r, db, &k, &d), MDB_NOTFOUND);
	mdb_txn_abort(r);
	mdb_env_close(small_env);
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
	RC(mdb_env_copyfd(env, COMPAT_NO_FILEHANDLE), NO_FILE_ERROR);
	RC(mdb_env_copyfd2(env, COMPAT_NO_FILEHANDLE, MDB_CP_COMPACT), NO_FILE_ERROR);
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
	test_comparator_after_cursor_open();
	test_comparator_reuse();
	test_errors();
	test_threads();
	test_drop();
	test_nested();
	test_copy("cursors");
	test_copy("put");
	test_dupfixed_small();
	test_copy("small");
	test_count_after_other_put();
	test_moves_after_failed_search();

	printf("capi_write: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
