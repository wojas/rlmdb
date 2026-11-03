/* The 13 functions LMDB 1.0's lmdb.h declares and 0.9's does not.
 *
 *   capi_v10 <scratch-dir>
 *
 * Built three times: against C LMDB 1.0 with its lmdb.h (the oracle),
 * against rlmdb's LMDB 1.0 stub with the same header (-DLMDB_RUST; a
 * program built for C 1.0), and against librlmdb with rlmdb's 0.9 lmdb.h
 * and rlmdb.h (-DLMDB_RUST; rlmdb.h declares the 13 functions), which
 * asks for LMDB 1.0 environments with rlmdb_env_set_format. Every
 * environment is LMDB 1.0.
 *
 * mdb_env_set_pagesize, mdb_txn_flags and mdb_cursor_is_db are
 * implemented: every build must pass the same checks and print the same
 * trace ("T ..." lines). The others are not implemented in rlmdb: the
 * Rust builds check that they return ENOTSUP after C 1.0's argument
 * checks and change nothing; DIFFERS() checks the C result in the C build
 * and ENOTSUP in the Rust builds; RUST_ONLY() marks calls that C 1.0
 * cannot take (it would crash on the NULL argument) or that would change
 * the C build's environments.
 *
 * Prints "capi_v10: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
/* mkdir, open, file handles; their Windows forms (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"
#if MDB_VERSION_MAJOR < 1
/* rlmdb's 0.9 lmdb.h: the 1.0 functions come from rlmdb.h. */
#include "rlmdb.h"
#endif

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)
#define RC(expr, expected) do { int rc_ = (expr), ex_ = (expected); checks++; \
	if (rc_ != ex_) { failures++; \
		fprintf(stderr, "%s:%d: %s returned %d (%s), expected %d (%s)\n", \
			__FILE__, __LINE__, #expr, rc_, mdb_strerror(rc_), ex_, mdb_strerror(ex_)); \
	} } while (0)
#define OK(expr) RC(expr, 0)

#ifdef LMDB_RUST
/* A function rlmdb does not implement: C returns c_rc, rlmdb ENOTSUP. */
#define DIFFERS(expr, c_rc) RC(expr, ENOTSUP)
#define RUST_ONLY(stmt) do { stmt; } while (0)
#else
#define DIFFERS(expr, c_rc) RC(expr, c_rc)
#define RUST_ONLY(stmt) do { } while (0)
#endif

/* A trace line, compared between the builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

static char scratch[1024];

static MDB_val sv(const char *s)
{
	MDB_val v;
	v.mv_size = strlen(s);
	v.mv_data = (void *)s;
	return v;
}

/* A key for the trace: printable bytes as they are, others as \xx. */
static const char *show(MDB_val v)
{
	static char buf[256];
	size_t i, n = 0;
	for (i = 0; i < v.mv_size && n + 4 < sizeof(buf); i++) {
		unsigned char c = ((unsigned char *)v.mv_data)[i];
		if (c >= 0x20 && c < 0x7f && c != '\\')
			buf[n++] = (char)c;
		else
			n += (size_t)snprintf(buf + n, sizeof(buf) - n, "\\%02x", c);
	}
	buf[n] = 0;
	return buf;
}

static void env_path(char *path, size_t size, const char *name)
{
	snprintf(path, size, "%s/%s", scratch, name);
	mkdir(path, 0775);
}

/* An environment handle that will be LMDB 1.0: the 1.0 libraries create
 * 1.0 environments; librlmdb (0.9 header) is asked for one. */
static MDB_env *create_env(void)
{
	MDB_env *env;
	OK(mdb_env_create(&env));
#if MDB_VERSION_MAJOR < 1
	OK(rlmdb_env_set_format(env, RLMDB_FORMAT_10));
#endif
	return env;
}

/* A fresh LMDB 1.0 environment in <scratch>/<name>. */
static MDB_env *new_env(const char *name, MDB_dbi maxdbs)
{
	char path[1200];
	MDB_env *env = create_env();
	env_path(path, sizeof(path), name);
	OK(mdb_env_set_mapsize(env, 8u << 20));
	OK(mdb_env_set_maxdbs(env, maxdbs));
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	return env;
}

static void put(MDB_txn *txn, MDB_dbi dbi, const char *k, const char *d)
{
	MDB_val key = sv(k), data = sv(d);
	OK(mdb_put(txn, dbi, &key, &data, 0));
}

static int has_key(MDB_env *env, const char *k)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key = sv(k), data;
	int rc;
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	rc = mdb_get(txn, dbi, &key, &data);
	mdb_txn_abort(txn);
	return rc == 0;
}

/* ---------------------------------------------------------------- */
/* mdb_env_set_pagesize                                              */
/* ---------------------------------------------------------------- */

static void test_set_pagesize(void)
{
	static const int bad[] = { 0, 1, 128, 255, 257, 384, 1000, 4097, 65537, 65535,
		131072, 1 << 30, INT_MAX, -1, -256, -4096, INT_MIN };
	static const int good[] = { 256, 512, 1024, 4096, 16384, 32768, 65536 };
	char path[1200];
	MDB_env *env;
	MDB_stat st;
	size_t i;

	RC(mdb_env_set_pagesize(NULL, 4096), EINVAL);
	env = create_env();
	for (i = 0; i < sizeof(bad) / sizeof(bad[0]); i++)
		RC(mdb_env_set_pagesize(env, bad[i]), EINVAL);
	for (i = 0; i < sizeof(good) / sizeof(good[0]); i++)
		OK(mdb_env_set_pagesize(env, good[i]));
	/* The last valid size counts; a refused one changes nothing. */
	OK(mdb_env_set_pagesize(env, 1024));
	RC(mdb_env_set_pagesize(env, 3000), EINVAL);
	env_path(path, sizeof(path), "pagesize");
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_env_stat(env, &st));
	T("pagesize: new environment with 1024-byte pages: ms_psize %u", st.ms_psize);
	/* After the open: EINVAL, also for a valid size. */
	RC(mdb_env_set_pagesize(env, 4096), EINVAL);
	RC(mdb_env_set_pagesize(env, 1024), EINVAL);
	mdb_env_close(env);

	/* An existing environment keeps its page size. */
	env = create_env();
	OK(mdb_env_set_pagesize(env, 512));
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_env_stat(env, &st));
	T("pagesize: existing environment opened with 512: ms_psize %u", st.ms_psize);
	mdb_env_close(env);

	/* The smallest and the largest. */
	for (i = 0; i < 2; i++) {
		int size = i ? 65536 : 256;
		MDB_txn *txn;
		MDB_dbi dbi;
		env = create_env();
		OK(mdb_env_set_pagesize(env, size));
		env_path(path, sizeof(path), i ? "pagesize-64k" : "pagesize-256");
		OK(mdb_env_set_mapsize(env, 8u << 20));
		OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
		OK(mdb_txn_begin(env, NULL, 0, &txn));
		OK(mdb_dbi_open(txn, NULL, 0, &dbi));
		put(txn, dbi, "key", "value");
		OK(mdb_txn_commit(txn));
		OK(mdb_env_stat(env, &st));
		T("pagesize: %d: ms_psize %u, entries %zu, maxkeysize %d", size, st.ms_psize,
			(size_t)st.ms_entries, mdb_env_get_maxkeysize(env));
		mdb_env_close(env);
	}

#if defined(LMDB_RUST) && MDB_VERSION_MAJOR < 1
	/* librlmdb without a request for 1.0: a new environment is 0.9,
	 * which has no page-size setting. */
	OK(mdb_env_create(&env));
	OK(mdb_env_set_pagesize(env, 4096));
	env_path(path, sizeof(path), "pagesize-09");
	RC(mdb_env_open(env, path, MDB_NOTLS, 0664), EINVAL);
	mdb_env_close(env);
	/* rlmdb_env_set_pagesize is the same function under rlmdb's name. */
	env = create_env();
	RC(rlmdb_env_set_pagesize(env, 255), EINVAL);
	OK(rlmdb_env_set_pagesize(env, 2048));
	env_path(path, sizeof(path), "pagesize-rlmdb");
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_env_stat(env, &st));
	CHECK(st.ms_psize == 2048);
	RC(rlmdb_env_set_pagesize(env, 4096), EINVAL);
	mdb_env_close(env);
#endif
}

/* ---------------------------------------------------------------- */
/* mdb_txn_flags                                                     */
/* ---------------------------------------------------------------- */

static void trace_txn_flags(const char *what, MDB_txn *txn)
{
	unsigned flags = 0xdead;
	OK(mdb_txn_flags(txn, &flags));
	T("txn_flags: %s: %#x", what, flags);
}

static void test_txn_flags(void)
{
	MDB_env *env = new_env("txn-flags", 4);
	MDB_txn *r, *w, *child, *grandchild;
	MDB_dbi dbi;
	unsigned flags = 0xdead;

	RC(mdb_txn_flags(NULL, &flags), EINVAL);
	CHECK(flags == 0xdead);

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	trace_txn_flags("read-only", r);
	RUST_ONLY(RC(mdb_txn_flags(r, NULL), EINVAL));
	mdb_txn_reset(r);
	trace_txn_flags("read-only, reset", r);
	OK(mdb_txn_renew(r));
	trace_txn_flags("read-only, renewed", r);
	mdb_txn_abort(r);

	OK(mdb_txn_begin(env, NULL, 0, &w));
	trace_txn_flags("write", w);
	OK(mdb_dbi_open(w, NULL, 0, &dbi));
	put(w, dbi, "a", "1");
	OK(mdb_txn_begin(env, w, 0, &child));
	trace_txn_flags("nested", child);
	trace_txn_flags("parent of an active child", w);
	OK(mdb_txn_begin(env, child, 0, &grandchild));
	trace_txn_flags("nested, two levels", grandchild);
	put(grandchild, dbi, "b", "2");
	OK(mdb_txn_commit(grandchild));
	mdb_txn_abort(child);
	trace_txn_flags("write, after its child", w);
	OK(mdb_txn_commit(w));
	mdb_env_close(env);

	/* A read-only transaction of a read-only environment. */
	{
		char path[1200];
		MDB_env *ro = create_env();
		env_path(path, sizeof(path), "txn-flags");
		OK(mdb_env_open(ro, path, MDB_RDONLY | MDB_NOTLS, 0664));
		OK(mdb_txn_begin(ro, NULL, MDB_RDONLY, &r));
		trace_txn_flags("read-only environment", r);
		mdb_txn_abort(r);
		mdb_env_close(ro);
	}
}

/* ---------------------------------------------------------------- */
/* mdb_cursor_is_db                                                  */
/* ---------------------------------------------------------------- */

/* Walks the keys of the database of `cur` forwards and backwards and
 * traces each with what mdb_cursor_is_db says, then where the cursor is
 * left. */
static void walk(const char *what, MDB_cursor *cur)
{
	MDB_val key, data;
	int rc;
	T("is_db: %s: unpositioned: %d", what, mdb_cursor_is_db(cur));
	for (rc = mdb_cursor_get(cur, &key, &data, MDB_FIRST); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_NODUP))
		T("is_db: %s: next %s: %d", what, show(key), mdb_cursor_is_db(cur));
	RC(rc, MDB_NOTFOUND);
	T("is_db: %s: after the last: %d", what, mdb_cursor_is_db(cur));
	for (rc = mdb_cursor_get(cur, &key, &data, MDB_LAST); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, MDB_PREV_NODUP))
		T("is_db: %s: prev %s: %d", what, show(key), mdb_cursor_is_db(cur));
	RC(rc, MDB_NOTFOUND);
	T("is_db: %s: before the first: %d", what, mdb_cursor_is_db(cur));
}

/* Positions `cur` with MDB_SET_RANGE / MDB_SET on `k` and traces. */
static void seek(const char *what, MDB_cursor *cur, MDB_cursor_op op, const char *k)
{
	MDB_val key = sv(k), data;
	int rc = mdb_cursor_get(cur, &key, &data, op);
	T("is_db: %s: %s %s: rc %d, %s: %d", what, op == MDB_SET ? "set" : "set_range", k,
		rc, rc ? "-" : show(key), mdb_cursor_is_db(cur));
}

static void test_cursor_is_db(void)
{
	MDB_env *env = new_env("is-db", 8);
	MDB_txn *txn;
	MDB_dbi main_dbi, a, m, dups, zzz;
	MDB_cursor *cur, *wcur;
	MDB_val key, data;
	char buf[32];
	int i;

	CHECK(mdb_cursor_is_db(NULL) == 0);

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &main_dbi));
	/* Plain items in the main DB, between and after named databases. */
	put(txn, main_dbi, "k1", "v1");
	put(txn, main_dbi, "n", "v2");
	OK(mdb_dbi_open(txn, "a", MDB_CREATE, &a));
	put(txn, a, "x", "1");
	put(txn, a, "y", "2");
	OK(mdb_dbi_open(txn, "m", MDB_CREATE, &m));
	put(txn, m, "only", "1");
	/* A DUPSORT database: a key with a few duplicates (a sub-page) and
	 * one with enough for a sub-database. */
	OK(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dups));
	put(txn, dups, "few", "1");
	put(txn, dups, "few", "2");
	for (i = 0; i < 400; i++) {
		snprintf(buf, sizeof(buf), "dup%04d-padding-padding", i);
		put(txn, dups, "many", buf);
	}
	put(txn, dups, "one", "1");
	/* The last item of the main DB is a named database. */
	OK(mdb_dbi_open(txn, "zzz", MDB_CREATE, &zzz));
	put(txn, zzz, "z", "1");

	/* In the write transaction (the core's tracked cursors). */
	OK(mdb_cursor_open(txn, main_dbi, &wcur));
	walk("write txn, main", wcur);
	seek("write txn, main", wcur, MDB_SET, "n");
	key = sv("m");
	data.mv_size = 0;
	/* mdb_dbi_open stored the name with its NUL (LMDB 1.0). */
	key.mv_size = 2;
	RC(mdb_cursor_get(wcur, &key, &data, MDB_SET), 0);
	T("is_db: write txn, main: set m\\00: %d", mdb_cursor_is_db(wcur));
	mdb_cursor_close(wcur);
	OK(mdb_cursor_open(txn, dups, &wcur));
	walk("write txn, dups", wcur);
	mdb_cursor_close(wcur);
	OK(mdb_txn_commit(txn));

	/* In a read-only transaction. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_cursor_open(txn, main_dbi, &cur));
	walk("read txn, main", cur);
	seek("read txn, main", cur, MDB_SET_RANGE, "b");
	seek("read txn, main", cur, MDB_SET_RANGE, "zz");
	seek("read txn, main", cur, MDB_SET, "k1");
	seek("read txn, main", cur, MDB_SET, "l");
	mdb_cursor_close(cur);
	OK(mdb_cursor_open(txn, a, &cur));
	walk("read txn, a", cur);
	mdb_cursor_close(cur);
	OK(mdb_cursor_open(txn, dups, &cur));
	walk("read txn, dups", cur);
	key = sv("many");
	OK(mdb_cursor_get(cur, &key, &data, MDB_SET));
	OK(mdb_cursor_get(cur, &key, &data, MDB_LAST_DUP));
	T("is_db: read txn, dups: last dup of many: %d", mdb_cursor_is_db(cur));
	mdb_cursor_close(cur);
	mdb_txn_abort(txn);

	/* A nested transaction's cursor, and a parent's cursor while a child
	 * is active. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_cursor_open(txn, main_dbi, &wcur));
	OK(mdb_cursor_get(wcur, &key, &data, MDB_FIRST));
	T("is_db: parent: first %s: %d", show(key), mdb_cursor_is_db(wcur));
	{
		MDB_txn *child;
		MDB_cursor *ccur;
		OK(mdb_txn_begin(env, txn, 0, &child));
		OK(mdb_cursor_open(child, main_dbi, &ccur));
		OK(mdb_cursor_get(ccur, &key, &data, MDB_LAST));
		T("is_db: child: last %s: %d", show(key), mdb_cursor_is_db(ccur));
		mdb_cursor_close(ccur);
		mdb_txn_abort(child);
	}
	T("is_db: parent after the child: %d", mdb_cursor_is_db(wcur));
	mdb_cursor_close(wcur);
	mdb_txn_abort(txn);

#ifdef LMDB_RUST
	/* A cursor whose read transaction was reset reads nothing (C would
	 * look at a page of the old snapshot). */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_cursor_open(txn, main_dbi, &cur));
	OK(mdb_cursor_get(cur, &key, &data, MDB_FIRST));
	CHECK(mdb_cursor_is_db(cur) == 1);
	mdb_txn_reset(txn);
	CHECK(mdb_cursor_is_db(cur) == 0);
	OK(mdb_txn_renew(txn));
	CHECK(mdb_cursor_is_db(cur) == 0);
	OK(mdb_cursor_renew(txn, cur));
	OK(mdb_cursor_get(cur, &key, &data, MDB_FIRST));
	CHECK(mdb_cursor_is_db(cur) == 1);
	mdb_txn_abort(txn);
	/* ... and so does one whose transaction ended. */
	CHECK(mdb_cursor_is_db(cur) == 0);
	mdb_cursor_close(cur);
#endif
	mdb_env_close(env);
}

/* ---------------------------------------------------------------- */
/* Two-phase commit: mdb_txn_prepare, mdb_env_rollback               */
/* ---------------------------------------------------------------- */

static void test_two_phase_commit(void)
{
	MDB_env *env = new_env("2pc", 4);
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_envinfo info;
	size_t last;

	RUST_ONLY(RC(mdb_txn_prepare(NULL), EINVAL));
	RUST_ONLY(RC(mdb_env_rollback(NULL, 1), EINVAL));

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	put(txn, dbi, "before", "1");
	/* C writes the data pages and only allows commit or abort; rlmdb
	 * returns ENOTSUP and the transaction stays an ordinary one. */
	DIFFERS(mdb_txn_prepare(txn), 0);
	RUST_ONLY(put(txn, dbi, "after", "2"));
	RUST_ONLY(DIFFERS(mdb_txn_prepare(txn), 0));
	OK(mdb_txn_commit(txn));
	T("2pc: committed after prepare: before %d", has_key(env, "before"));
	RUST_ONLY(CHECK(has_key(env, "after")));

	/* A read-only transaction (C commits it, which frees it). */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	DIFFERS(mdb_txn_prepare(txn), 0);
	RUST_ONLY(mdb_txn_abort(txn));

	/* A nested transaction. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	{
		MDB_txn *child;
		OK(mdb_txn_begin(env, txn, 0, &child));
		put(child, dbi, "nested", "3");
		RUST_ONLY(DIFFERS(mdb_txn_prepare(child), 0));
		OK(mdb_txn_commit(child));
	}
	OK(mdb_txn_commit(txn));
	T("2pc: nested: %d", has_key(env, "nested"));

	/* mdb_env_rollback: C refuses a txnid other than the last with
	 * MDB_CANT_ROLLBACK; rlmdb has no rollback. */
	OK(mdb_env_info(env, &info));
	last = info.me_last_txnid;
	DIFFERS(mdb_env_rollback(env, last + 1), MDB_CANT_ROLLBACK);
	DIFFERS(mdb_env_rollback(env, last - 1), MDB_CANT_ROLLBACK);
	/* With the last txnid C rolls the commit back; rlmdb keeps it. */
	RUST_ONLY(DIFFERS(mdb_env_rollback(env, last), 0));
	OK(mdb_env_info(env, &info));
	T("2pc: last txnid unchanged: %d, nested %d", info.me_last_txnid == last,
		has_key(env, "nested"));
	mdb_env_close(env);

#ifdef LMDB_RUST
	/* An environment that is not open: EINVAL (C would crash). */
	env = create_env();
	RC(mdb_env_rollback(env, 1), EINVAL);
	mdb_env_close(env);
#endif
}

/* ---------------------------------------------------------------- */
/* Encryption and checksums                                          */
/* ---------------------------------------------------------------- */

static int enc_calls, sum_calls, str2key_calls;

static int enc_func(const MDB_val *src, MDB_val *dst, const MDB_val *key, int encdec)
{
	(void)key; (void)encdec;
	enc_calls++;
	memcpy(dst->mv_data, src->mv_data, src->mv_size);
	return 0;
}

static void sum_func(const MDB_val *src, MDB_val *dst, const MDB_val *key)
{
	(void)src; (void)key;
	sum_calls++;
	memset(dst->mv_data, 0, dst->mv_size);
}

static int str2key(const char *passwd, MDB_val *key)
{
	(void)passwd; (void)key;
	str2key_calls++;
	return 0;
}

static void test_crypto(void)
{
	char keybytes[32] = "0123456789abcdef0123456789abcdef";
	MDB_val key;
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	char path[1200];

	key.mv_size = sizeof(keybytes);
	key.mv_data = keybytes;

	/* C 1.0's checks: NULL arguments, a size of 0 for checksums. */
	RC(mdb_env_set_encrypt(NULL, enc_func, &key, 0), EINVAL);
	RC(mdb_env_set_checksum(NULL, sum_func, 8), EINVAL);
	env = create_env();
	RC(mdb_env_set_encrypt(env, NULL, &key, 0), EINVAL);
	RC(mdb_env_set_encrypt(env, enc_func, NULL, 0), EINVAL);
	RC(mdb_env_set_checksum(env, NULL, 8), EINVAL);
	RC(mdb_env_set_checksum(env, sum_func, 0), EINVAL);
	mdb_env_close(env);

	/* After the open: EINVAL. */
	env = new_env("crypto-open", 4);
	RC(mdb_env_set_encrypt(env, enc_func, &key, 0), EINVAL);
	RC(mdb_env_set_checksum(env, sum_func, 8), EINVAL);
	mdb_env_close(env);

	/* Valid calls: C sets encryption and checksums up; rlmdb returns
	 * ENOTSUP and the environment is opened plain. */
	env = create_env();
	DIFFERS(mdb_env_set_encrypt(env, enc_func, &key, 16), 0);
	DIFFERS(mdb_env_set_checksum(env, sum_func, 8), 0);
#ifdef LMDB_RUST
	env_path(path, sizeof(path), "crypto-refused");
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	put(txn, dbi, "plain", "1");
	OK(mdb_txn_commit(txn));
	CHECK(has_key(env, "plain"));
#else
	(void)path; (void)txn; (void)dbi;
#endif
	mdb_env_close(env);
	CHECK(enc_calls == 0 && sum_calls == 0);
}

/* ---------------------------------------------------------------- */
/* Incremental backups                                               */
/* ---------------------------------------------------------------- */

static void test_incremental(void)
{
#ifdef LMDB_RUST
	MDB_env *env;
	char path[1200], file[1300];
	int fd;

	RC(mdb_env_incr_dumpfd(NULL, COMPAT_NO_FILEHANDLE, 1), EINVAL);
	RC(mdb_env_incr_dump(NULL, "x", 1), EINVAL);
	RC(mdb_env_incr_loadfd(NULL, COMPAT_NO_FILEHANDLE), EINVAL);
	/* Not open: EINVAL (C would crash). */
	env = create_env();
	RC(mdb_env_incr_dumpfd(env, COMPAT_NO_FILEHANDLE, 1), EINVAL);
	RC(mdb_env_incr_dump(env, "x", 1), EINVAL);
	RC(mdb_env_incr_loadfd(env, COMPAT_NO_FILEHANDLE), EINVAL);
	mdb_env_close(env);

	env = new_env("incr", 4);
	RC(mdb_env_incr_dump(env, NULL, 1), EINVAL);
	snprintf(path, sizeof(path), "%s/incr", scratch);
	snprintf(file, sizeof(file), "%s/incr.dump", scratch);
	RC(mdb_env_incr_dump(env, file, 1), ENOTSUP);
	{
		FILE *f = fopen(file, "rb");
		CHECK(f == NULL);
		if (f)
			fclose(f);
	}
	fd = open(file, O_RDWR | O_CREAT | O_TRUNC | O_BINARY, COMPAT_CREATE_MODE);
	CHECK(fd >= 0);
	RC(mdb_env_incr_dumpfd(env, COMPAT_FILEHANDLE(fd), 1), ENOTSUP);
	CHECK(compat_filehandle_size(COMPAT_FILEHANDLE(fd)) == 0);
	RC(mdb_env_incr_loadfd(env, COMPAT_FILEHANDLE(fd)), ENOTSUP);
	close(fd);
	/* The environment is untouched: still a directory environment that
	 * reads and writes (C's mdb_env_incr_dump sets MDB_NOSUBDIR). */
	{
		unsigned flags;
		MDB_txn *txn;
		MDB_dbi dbi;
		OK(mdb_env_get_flags(env, &flags));
		CHECK(!(flags & MDB_NOSUBDIR));
		OK(mdb_txn_begin(env, NULL, 0, &txn));
		OK(mdb_dbi_open(txn, NULL, 0, &dbi));
		put(txn, dbi, "k", "v");
		OK(mdb_txn_commit(txn));
		CHECK(has_key(env, "k"));
	}
	(void)path;
	mdb_env_close(env);
#endif
}

/* ---------------------------------------------------------------- */
/* Crypto modules: mdb_modload, mdb_modunload, mdb_modsetup          */
/* ---------------------------------------------------------------- */

static void test_modules(void)
{
	char path[1200];
	MDB_crypto_funcs funcs, *mcf = (MDB_crypto_funcs *)&funcs;
	char *errmsg = NULL;
	void *handle;
	MDB_env *env;

	/* A module that does not exist: NULL and a message, *mcf_ptr as it
	 * was (C: dlerror()'s text; rlmdb loads nothing). */
	snprintf(path, sizeof(path), "%s/no-such-module.so", scratch);
	handle = mdb_modload(path, NULL, &mcf, &errmsg);
	CHECK(handle == NULL);
	CHECK(errmsg != NULL && errmsg[0] != 0);
	CHECK(mcf == &funcs);
	T("modload: missing module: NULL %d, message %d", handle == NULL, errmsg != NULL);
#ifdef LMDB_RUST
	/* Any module, also with a symbol name: NULL and rlmdb's message. */
	errmsg = NULL;
	CHECK(mdb_modload(path, "MDB_crypto", &mcf, &errmsg) == NULL);
	CHECK(errmsg != NULL && strstr(errmsg, "rlmdb") != NULL);
	CHECK(mdb_modload(NULL, NULL, NULL, NULL) == NULL);
	mdb_modunload(NULL);

	/* mdb_modsetup: what C would set up makes mdb_env_open refuse the
	 * environment (ENOTSUP), as the program cannot see a failure. */
	memset(&funcs, 0, sizeof(funcs));
	funcs.mcf_str2key = str2key;
	funcs.mcf_encfunc = enc_func;
	funcs.mcf_sumfunc = sum_func;
	funcs.mcf_keysize = 32;
	funcs.mcf_sumsize = 8;
	env = create_env();
	mdb_modsetup(env, &funcs, "secret");
	env_path(path, sizeof(path), "modsetup-both");
	RC(mdb_env_open(env, path, MDB_NOTLS, 0664), ENOTSUP);
	mdb_env_close(env);
	/* Checksums only (no passphrase). */
	env = create_env();
	mdb_modsetup(env, &funcs, NULL);
	env_path(path, sizeof(path), "modsetup-sum");
	RC(mdb_env_open(env, path, MDB_NOTLS, 0664), ENOTSUP);
	mdb_env_close(env);
	/* Encryption only. */
	funcs.mcf_sumfunc = NULL;
	env = create_env();
	mdb_modsetup(env, &funcs, "secret");
	env_path(path, sizeof(path), "modsetup-enc");
	RC(mdb_env_open(env, path, MDB_NOTLS, 0664), ENOTSUP);
	mdb_env_close(env);
	/* Nothing C would set up: an encryption function without a
	 * passphrase, a checksum size of 0, NULL arguments. */
	funcs.mcf_sumfunc = sum_func;
	funcs.mcf_sumsize = 0;
	env = create_env();
	mdb_modsetup(env, &funcs, NULL);
	mdb_modsetup(env, NULL, "secret");
	mdb_modsetup(NULL, &funcs, "secret");
	env_path(path, sizeof(path), "modsetup-none");
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	/* On an open environment: nothing (C's calls fail there). */
	funcs.mcf_sumsize = 8;
	mdb_modsetup(env, &funcs, "secret");
	CHECK(has_key(env, "x") == 0);
	mdb_env_close(env);
	CHECK(enc_calls == 0 && sum_calls == 0 && str2key_calls == 0);
#else
	(void)env;
#endif
}

int main(int argc, char **argv)
{
	int major, minor, patch;
	if (argc != 2) {
		fprintf(stderr, "usage: capi_v10 <scratch-dir>\n");
		return 2;
	}
	snprintf(scratch, sizeof(scratch), "%s", argv[1]);
	mdb_version(&major, &minor, &patch);
	/* The C 1.0 oracle and the 1.0 stub say 1.0.x, librlmdb 0.9.85. */
	printf("library: LMDB %d.%d.%d\n", major, minor, patch);

	test_set_pagesize();
	test_txn_flags();
	test_cursor_is_db();
	test_two_phase_commit();
	test_crypto();
	test_incremental();
	test_modules();

	printf("capi_v10: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
