/* Two-phase commit: mdb_txn_prepare and mdb_env_rollback (LMDB 1.0).
 *
 *   capi_2pc <scratch-dir> <format: 0.9|1.0>
 *
 * Built three times, as capi_v10.c: against C LMDB 1.0 with its lmdb.h
 * (the oracle), against rlmdb's LMDB 1.0 stub with the same header
 * (-DLMDB_RUST) and against librlmdb with rlmdb's lmdb.h and rlmdb.h
 * (-DLMDB_RUST), which runs it on 1.0 environments (rlmdb_env_set_format)
 * and on 0.9 ones (C 0.9 has no two-phase commit; rlmdb's works on both).
 *
 * Every build prints the same trace ("T ..." lines): return codes of
 * every call on a prepared transaction, what readers and the next writer
 * see, the rollback's results. RUST_ONLY() marks what C 1.0 cannot run:
 * where it crashes (NULL handles, a cursor it freed at the prepare),
 * frees memory it does not own (mdb_txn_commit and mdb_txn_abort after a
 * prepare that ended the transaction, KNOWN_C_BUGS.md row 61) or breaks
 * the environment (a rollback C allows but should not, rows 62 and 63).
 *
 * The environments are left in <scratch>/<name>; after the prepare of
 * "commit" and of "abort", the data file is copied to prepared.mdb in
 * the same directory (the file between the two phases), and the test
 * compares them with the oracle's byte for byte.
 *
 * Prints "capi_2pc: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* mkdir; its Windows form (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"
#if defined(LMDB_RUST) && MDB_VERSION_MAJOR < 1
/* rlmdb's 0.9 lmdb.h: the 1.0 functions, codes and the format switch
 * come from rlmdb.h. */
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
#define RUST_ONLY(stmt) do { stmt; } while (0)
#else
#define RUST_ONLY(stmt) do { } while (0)
#endif

/* A trace line, compared between the builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)
/* A traced return code. */
#define TRC(what, expr) T("%s: %d", what, (expr))

static char scratch[1024];
static int v09;

static MDB_val sv(const char *s)
{
	MDB_val v;
	v.mv_size = strlen(s);
	v.mv_data = (void *)s;
	return v;
}

static void env_path(char *path, size_t size, const char *name)
{
	snprintf(path, size, "%s/%s", scratch, name);
	mkdir(path, 0775);
}

/* A new environment handle of the format asked for: the 1.0 libraries
 * create 1.0 environments, librlmdb is told which. */
static MDB_env *create_env(void)
{
	MDB_env *env;
	OK(mdb_env_create(&env));
#ifdef RLMDB_FORMAT_10
	OK(rlmdb_env_set_format(env, v09 ? RLMDB_FORMAT_09 : RLMDB_FORMAT_10));
#endif
	return env;
}

/* A fresh environment in <scratch>/<name>, map of <mapsize> bytes. */
static MDB_env *new_env(const char *name, size_t mapsize, unsigned int flags)
{
	char path[1200];
	MDB_env *env = create_env();
	env_path(path, sizeof(path), name);
	OK(mdb_env_set_mapsize(env, mapsize));
	OK(mdb_env_set_maxdbs(env, 4));
	OK(mdb_env_open(env, path, flags | MDB_NOTLS, 0664));
	return env;
}

static void put(MDB_txn *txn, MDB_dbi dbi, const char *k, const char *d)
{
	MDB_val key = sv(k), data = sv(d);
	OK(mdb_put(txn, dbi, &key, &data, 0));
}

/* Commits a transaction putting k (in a fresh write transaction);
 * returns its id. */
static size_t commit_put(MDB_env *env, const char *k, const char *d)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	size_t id;
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	put(txn, dbi, k, d);
	id = mdb_txn_id(txn);
	OK(mdb_txn_commit(txn));
	return id;
}

/* The value of k in a new read transaction, "-" if there is none (in
 * one of four buffers, so that a trace line can show several). */
static const char *value(MDB_env *env, const char *k)
{
	static char bufs[4][64];
	static int next;
	char *buf = bufs[next++ % 4];
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key = sv(k), data;
	int rc;
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	rc = mdb_get(txn, dbi, &key, &data);
	if (rc)
		snprintf(buf, sizeof(bufs[0]), "-");
	else
		snprintf(buf, sizeof(bufs[0]), "%.*s", (int)data.mv_size, (char *)data.mv_data);
	mdb_txn_abort(txn);
	return buf;
}

static size_t last_txnid(MDB_env *env)
{
	MDB_envinfo info;
	OK(mdb_env_info(env, &info));
	return info.me_last_txnid;
}

/* Copies <scratch>/<name>/data.mdb to prepared.mdb next to it. */
static void copy_data_file(const char *name)
{
	char src[1200], dst[1200];
	static char buf[1 << 16];
	FILE *in, *out;
	size_t n;
	snprintf(src, sizeof(src), "%s/%s/data.mdb", scratch, name);
	snprintf(dst, sizeof(dst), "%s/%s/prepared.mdb", scratch, name);
	in = fopen(src, "rb");
	out = fopen(dst, "wb");
	CHECK(in && out);
	if (!in || !out)
		return;
	while ((n = fread(buf, 1, sizeof(buf), in)) > 0)
		CHECK(fwrite(buf, 1, n, out) == n);
	fclose(in);
	fclose(out);
}

static int cmp_rev(const MDB_val *a, const MDB_val *b)
{
	size_t n = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;
	int c = memcmp(b->mv_data, a->mv_data, n);
	return c ? c : (a->mv_size > b->mv_size) - (a->mv_size < b->mv_size);
}

/* ---------------------------------------------------------------- */
/* A prepared transaction: every call, then commit                   */
/* ---------------------------------------------------------------- */

static void test_prepare_commit(void)
{
	MDB_env *env = new_env("commit", 8u << 20, 0);
	MDB_txn *txn, *child, *reader;
	MDB_dbi main_dbi, named, other;
	MDB_cursor *cur, *cur2;
	MDB_val key = sv("k"), data = sv("v"), out;
	MDB_stat st;
	unsigned int flags;
	size_t id;

	RUST_ONLY(RC(mdb_txn_prepare(NULL), EINVAL));

	/* A named database that exists before. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "other", MDB_CREATE | MDB_DUPSORT, &other));
	OK(mdb_txn_commit(txn));
	commit_put(env, "a", "1");

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &reader));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &main_dbi));
	OK(mdb_dbi_open(txn, "named", MDB_CREATE, &named));
	put(txn, main_dbi, "a", "2");
	put(txn, main_dbi, "b", "2");
	put(txn, named, "x", "y");
	OK(mdb_cursor_open(txn, main_dbi, &cur));
	OK(mdb_cursor_open(txn, named, &cur2));
	OK(mdb_cursor_get(cur, &out, &data, MDB_FIRST));
	id = mdb_txn_id(txn);
	T("commit: txnid %zu", id);
	TRC("prepare", mdb_txn_prepare(txn));
	copy_data_file("commit");

	/* Everything that checks MDB_TXN_BLOCKED, after its handle checks. */
	TRC("put", mdb_put(txn, main_dbi, &key, &data, 0));
	TRC("put bad dbi", mdb_put(txn, 77, &key, &data, 0));
	TRC("put bad flags", mdb_put(txn, main_dbi, &key, &data, 0x80000000u));
	TRC("get", mdb_get(txn, main_dbi, &key, &out));
	TRC("get bad dbi", mdb_get(txn, 77, &key, &out));
	TRC("del", mdb_del(txn, main_dbi, &key, NULL));
	TRC("del bad dbi", mdb_del(txn, 77, &key, NULL));
	TRC("drop", mdb_drop(txn, named, 0));
	TRC("drop bad dbi", mdb_drop(txn, 77, 0));
	TRC("drop bad del", mdb_drop(txn, named, 2));
	TRC("cursor_open", mdb_cursor_open(txn, main_dbi, &cur));
	TRC("cursor_open bad dbi", mdb_cursor_open(txn, 77, &cur));
	TRC("dbi_open main", mdb_dbi_open(txn, NULL, 0, &main_dbi));
	TRC("dbi_open named", mdb_dbi_open(txn, "named", 0, &named));
	TRC("dbi_open new", mdb_dbi_open(txn, "new", MDB_CREATE, &other));
	TRC("dbi_open bad flags", mdb_dbi_open(txn, "new", 0x80000000u, &other));
	TRC("stat", mdb_stat(txn, main_dbi, &st));
	TRC("stat bad dbi", mdb_stat(txn, 77, &st));
	TRC("begin child", mdb_txn_begin(env, txn, 0, &child));
	TRC("renew", mdb_txn_renew(txn));
	mdb_txn_reset(txn);
	TRC("prepare again", mdb_txn_prepare(txn));
	/* What still works. */
	flags = 12345;
	TRC("dbi_flags", mdb_dbi_flags(txn, other, &flags));
	T("dbi_flags: %#x", flags);
	TRC("dbi_flags bad dbi", mdb_dbi_flags(txn, 77, &flags));
	TRC("set_relctx", mdb_set_relctx(txn, named, NULL));
	TRC("set_relctx bad dbi", mdb_set_relctx(txn, 77, NULL));
	{
		MDB_val a = sv("a"), b = sv("b");
		T("cmp: %d", mdb_cmp(txn, main_dbi, &a, &b) < 0);
		T("dcmp: %d", mdb_dcmp(txn, other, &a, &b) < 0);
		/* mdb_set_dupsort sets the handle's function (the database
		 * is not used again). */
		TRC("set_dupsort", mdb_set_dupsort(txn, other, cmp_rev));
		T("dcmp after set_dupsort: %d", mdb_dcmp(txn, other, &a, &b) > 0);
	}
	T("txn_id: %d", mdb_txn_id(txn) == id);
	TRC("txn_flags", mdb_txn_flags(txn, &flags));
	T("txn_flags: %#x", flags);
	T("txn_env: %d", mdb_txn_env(txn) == env);
	TRC("set_mapsize", mdb_env_set_mapsize(env, 16u << 20));
	/* The cursors were closed (C freed them; rlmdb's are detached and
	 * mdb_cursor_close frees them). */
	RUST_ONLY(RC(mdb_cursor_get(cur, &out, &data, MDB_NEXT), EINVAL));
	RUST_ONLY(RC(mdb_cursor_put(cur, &key, &data, 0), EINVAL));
	RUST_ONLY(RC(mdb_cursor_del(cur, 0), EINVAL));
	RUST_ONLY(CHECK(mdb_cursor_is_db(cur) == 0));
	RUST_ONLY(CHECK(mdb_cursor_txn(cur) == txn));
	RUST_ONLY(RC(mdb_cursor_renew(reader, cur), EINVAL));
	RUST_ONLY(mdb_cursor_close(cur));
	/* (cur2 is freed with the transaction.) */
	(void)cur2;

	/* Readers see the snapshot before. */
	T("between: a %s", value(env, "a"));
	T("between: b %s", value(env, "b"));
	T("between: last txnid is the one before: %d", last_txnid(env) == id - 1);

	TRC("commit", mdb_txn_commit(txn));
	T("after: a %s, b %s, last txnid %d", value(env, "a"), value(env, "b"),
		last_txnid(env) == id);
	/* The early reader kept its snapshot. */
	{
		MDB_dbi d;
		OK(mdb_dbi_open(reader, NULL, 0, &d));
		RC(mdb_get(reader, d, &key, &out), MDB_NOTFOUND);
		key = sv("a");
		OK(mdb_get(reader, d, &key, &out));
		T("early reader: a %.*s", (int)out.mv_size, (char *)out.mv_data);
		mdb_txn_abort(reader);
	}
	/* The named database is there. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	key = sv("x");
	TRC("named after commit", mdb_get(txn, named, &key, &out));
	mdb_txn_abort(txn);
	mdb_env_close(env);
}

/* ---------------------------------------------------------------- */
/* Prepare, then abort; an active child; read-only; nested            */
/* ---------------------------------------------------------------- */

static void test_prepare_abort(void)
{
	MDB_env *env = new_env("abort", 8u << 20, 0);
	MDB_txn *txn, *child;
	MDB_dbi dbi, named;
	MDB_cursor *cur;
	size_t id;

	commit_put(env, "a", "1");
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_dbi_open(txn, "named", MDB_CREATE, &named));
	put(txn, dbi, "a", "2");
	OK(mdb_cursor_open(txn, dbi, &cur));
	id = mdb_txn_id(txn);
	TRC("abort: prepare", mdb_txn_prepare(txn));
	copy_data_file("abort");
	mdb_txn_abort(txn);
	T("abort: a %s, last txnid %d", value(env, "a"), last_txnid(env) == id - 1);
	/* The created database's handle was closed. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	{
		MDB_val key = sv("x"), out;
		TRC("abort: named", mdb_get(txn, named, &key, &out));
	}
	mdb_txn_abort(txn);
	/* The next writer gets the same txnid. */
	T("abort: next txnid %d", commit_put(env, "b", "3") == id);
	T("abort: a %s, b %s", value(env, "a"), value(env, "b"));

	/* A child that is active is committed first. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_txn_begin(env, txn, 0, &child));
	put(child, dbi, "child", "4");
	TRC("child: prepare the parent", mdb_txn_prepare(txn));
	TRC("child: commit", mdb_txn_commit(txn));
	T("child: %s", value(env, "child"));

	/* A nested transaction is committed into its parent (and freed). */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_txn_begin(env, txn, 0, &child));
	put(child, dbi, "nested", "5");
	TRC("nested: prepare", mdb_txn_prepare(child));
	{
		MDB_val key = sv("nested"), out;
		TRC("nested: in the parent", mdb_get(txn, dbi, &key, &out));
	}
	T("nested: before the parent's commit: %s", value(env, "nested"));
	TRC("nested: commit", mdb_txn_commit(txn));
	T("nested: %s", value(env, "nested"));

	/* A read-only transaction is committed (and freed), also a reset
	 * one. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	TRC("read-only: prepare", mdb_txn_prepare(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	mdb_txn_reset(txn);
	TRC("reset: prepare", mdb_txn_prepare(txn));
	mdb_env_close(env);
}

/* ---------------------------------------------------------------- */
/* A prepare that ends the transaction: nothing to write, or failed  */
/* ---------------------------------------------------------------- */

static void test_prepare_ends(void)
{
	MDB_env *env = new_env("ends", 1u << 20, 0);
	MDB_txn *txn, *txn2, *child;
	MDB_dbi dbi;
	MDB_val key = sv("k"), data = sv("v"), out;
	char big[2000];
	int i, rc;

	commit_put(env, "a", "1");
	/* Nothing to write: an empty commit, which releases the writer
	 * lock. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	TRC("empty: prepare", mdb_txn_prepare(txn));
	TRC("empty: put", mdb_put(txn, dbi, &key, &data, 0));
	TRC("empty: get", mdb_get(txn, dbi, &key, &out));
	TRC("empty: dbi_open", mdb_dbi_open(txn, NULL, 0, &dbi));
	TRC("empty: begin child", mdb_txn_begin(env, txn, 0, &child));
	/* C reuses the same transaction for the next writer, and commit and
	 * abort of the ended one free it (row 61): only rlmdb goes on. */
	RUST_ONLY(RC(mdb_txn_prepare(txn), MDB_BAD_TXN));
	RUST_ONLY(OK(mdb_txn_begin(env, NULL, 0, &txn2)));
	RUST_ONLY(put(txn2, dbi, "meanwhile", "2"));
	RUST_ONLY(OK(mdb_txn_commit(txn2)));
	RUST_ONLY(OK(mdb_txn_commit(txn)));
	RUST_ONLY(CHECK(!strcmp(value(env, "meanwhile"), "2")));
	RUST_ONLY(OK(mdb_txn_begin(env, NULL, 0, &txn)));
	RUST_ONLY(OK(mdb_txn_prepare(txn)));
	RUST_ONLY(mdb_txn_abort(txn));

	/* A failed transaction: the prepare fails and aborts it. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	memset(big, 'x', sizeof(big));
	for (i = 0, rc = 0; i < 10000 && !rc; i++) {
		char k[16];
		MDB_val bk, bd;
		snprintf(k, sizeof(k), "%08d", i);
		bk = sv(k);
		bd.mv_size = sizeof(big);
		bd.mv_data = big;
		rc = mdb_put(txn, dbi, &bk, &bd, 0);
	}
	TRC("failed: put", rc);
	TRC("failed: prepare", mdb_txn_prepare(txn));
	TRC("failed: put after", mdb_put(txn, dbi, &key, &data, 0));
	TRC("failed: dbi_open after", mdb_dbi_open(txn, NULL, 0, &dbi));
	RUST_ONLY(RC(mdb_txn_prepare(txn), MDB_BAD_TXN));
	RUST_ONLY(RC(mdb_txn_commit(txn), MDB_BAD_TXN));
	/* The writer lock is free. */
	commit_put(env, "b", "2");
	T("failed: a %s, b %s", value(env, "a"), value(env, "b"));

	/* Ended by the prepare, then aborted (rlmdb frees the handle). */
	RUST_ONLY(OK(mdb_txn_begin(env, NULL, 0, &txn)));
	RUST_ONLY(OK(mdb_txn_prepare(txn)));
	RUST_ONLY(mdb_txn_abort(txn));
	mdb_env_close(env);
}

/* ---------------------------------------------------------------- */
/* mdb_env_rollback                                                  */
/* ---------------------------------------------------------------- */

static void test_rollback(void)
{
	MDB_env *env;
	MDB_txn *txn, *reader;
	MDB_dbi dbi, named;
	size_t t;

	RUST_ONLY(RC(mdb_env_rollback(NULL, 1), EINVAL));
	RUST_ONLY(env = create_env());
	RUST_ONLY(RC(mdb_env_rollback(env, 1), EINVAL));
	RUST_ONLY(mdb_env_close(env));

	/* A new environment, its first commit: nothing before. */
	env = new_env("rollback-first", 8u << 20, 0);
	TRC("first: rollback 0", mdb_env_rollback(env, 0));
	TRC("first: rollback 1", mdb_env_rollback(env, 1));
	t = commit_put(env, "a", "1");
	TRC("first: rollback of the first commit", mdb_env_rollback(env, t));
	mdb_env_close(env);

	/* An even txnid, after a prepared commit; twice in a row. */
	env = new_env("rollback-even", 8u << 20, 0);
	commit_put(env, "a", "1");
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_dbi_open(txn, "named", MDB_CREATE, &named));
	put(txn, dbi, "a", "2");
	put(txn, dbi, "b", "2");
	put(txn, named, "x", "y");
	t = mdb_txn_id(txn);
	T("even: txnid %zu", t);
	OK(mdb_txn_prepare(txn));
	OK(mdb_txn_commit(txn));
	TRC("even: rollback t+1", mdb_env_rollback(env, t + 1));
	TRC("even: rollback t-1", mdb_env_rollback(env, t - 1));
	TRC("even: rollback t", mdb_env_rollback(env, t));
	T("even: a %s, b %s, last txnid %d", value(env, "a"), value(env, "b"),
		last_txnid(env) == t - 1);
	TRC("even: rollback t again", mdb_env_rollback(env, t));
	TRC("even: rollback t-1 (twice in a row)", mdb_env_rollback(env, t - 1));
	/* The handle of the rolled-back database names nothing. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	{
		MDB_val key = sv("x"), data = sv("z"), out;
		TRC("even: named get", mdb_get(txn, named, &key, &out));
		TRC("even: named put", mdb_put(txn, named, &key, &data, 0));
		TRC("even: named open", mdb_dbi_open(txn, "named", MDB_CREATE, &dbi));
		T("even: same handle %d", dbi == named);
	}
	mdb_txn_abort(txn);
	/* The next writer takes the txnid again. */
	T("even: next txnid %d", commit_put(env, "c", "3") == t);
	T("even: a %s, b %s, c %s", value(env, "a"), value(env, "b"), value(env, "c"));
	mdb_env_close(env);

	/* An odd txnid: C 1.0 lets a second rollback through (row 62). */
	env = new_env("rollback-odd", 8u << 20, 0);
	commit_put(env, "a", "1");
	commit_put(env, "a", "2");
	t = commit_put(env, "a", "3");
	T("odd: txnid %zu", t);
	TRC("odd: rollback", mdb_env_rollback(env, t));
	T("odd: a %s, last txnid %d", value(env, "a"), last_txnid(env) == t - 1);
	RUST_ONLY(RC(mdb_env_rollback(env, t - 1), MDB_CANT_ROLLBACK));
	mdb_env_close(env);

	/* A reader of the snapshot stops the rollback in rlmdb (row 63). */
	env = new_env("rollback-reader", 8u << 20, 0);
	commit_put(env, "a", "1");
	t = commit_put(env, "a", "2");
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &reader));
	RUST_ONLY(RC(mdb_env_rollback(env, t), MDB_CANT_ROLLBACK));
	RUST_ONLY(CHECK(!strcmp(value(env, "a"), "2")));
	mdb_txn_reset(reader);
	TRC("reader: rollback after reset", mdb_env_rollback(env, t));
	OK(mdb_txn_renew(reader));
	T("reader: renewed on txnid %d", mdb_txn_id(reader) == t - 1);
	mdb_txn_abort(reader);
	mdb_env_close(env);

	/* MDB_NOSYNC and MDB_NOMETASYNC. */
	env = new_env("rollback-nosync", 8u << 20, MDB_NOSYNC);
	commit_put(env, "a", "1");
	t = commit_put(env, "a", "2");
	TRC("nosync: rollback", mdb_env_rollback(env, t));
	T("nosync: a %s", value(env, "a"));
	mdb_env_close(env);
	env = new_env("rollback-nometasync", 8u << 20, MDB_NOMETASYNC);
	commit_put(env, "a", "1");
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	put(txn, dbi, "a", "2");
	t = mdb_txn_id(txn);
	OK(mdb_txn_prepare(txn));
	OK(mdb_txn_commit(txn));
	TRC("nometasync: rollback", mdb_env_rollback(env, t));
	T("nometasync: a %s", value(env, "a"));
	mdb_env_close(env);

	/* A read-only environment: rlmdb refuses (C fails the write with
	 * EBADF and is unusable afterwards). */
	RUST_ONLY({
		char path[1200];
		env_path(path, sizeof(path), "rollback-first");
		env = create_env();
		OK(mdb_env_open(env, path, MDB_RDONLY, 0664));
		RC(mdb_env_rollback(env, 1), EACCES);
		mdb_env_close(env);
	});
}

int main(int argc, char **argv)
{
	int major, minor, patch;
	if (argc != 3 || (strcmp(argv[2], "0.9") && strcmp(argv[2], "1.0"))) {
		fprintf(stderr, "usage: capi_2pc <scratch-dir> <0.9|1.0>\n");
		return 2;
	}
	snprintf(scratch, sizeof(scratch), "%s", argv[1]);
	v09 = !strcmp(argv[2], "0.9");
	mdb_version(&major, &minor, &patch);
	printf("library: LMDB %d.%d.%d\n", major, minor, patch);

	test_prepare_commit();
	test_prepare_abort();
	test_prepare_ends();
	test_rollback();

	printf("capi_2pc: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
