/* MDB_PREVSNAPSHOT, LMDB 1.0's mdb_env_open flag.
 *
 *   capi_prevsnapshot <scratch-dir> <format: 0.9|1.0>
 *   capi_prevsnapshot <scratch-dir> <format> shared <env>
 *
 * A program written for LMDB 1.0's API. Built against C LMDB 1.0 (the
 * oracle), against rlmdb's LMDB 1.0 stub (-DLMDB_RUST; a program built for
 * C 1.0) and against librlmdb with rlmdb's headers (-DLMDB_RUST
 * -DLMDB_LIBRLMDB), which runs it on environments of the format asked for
 * (rlmdb_env_set_format; rlmdb takes the flag in both formats); the C
 * library and the stub make 1.0 environments. Every build must pass the
 * same checks and print the same trace ("T ..." lines), except where C 1.0
 * has a bug rlmdb does not port (KNOWN_C_BUGS.md rows 67, 69 and 71) or
 * where rlmdb refuses what C does not coordinate (two MDB_NOLOCK handles):
 * there BY_LIB() checks each build's own result, and the trace says
 * nothing.
 *
 * The first form runs alone. The test (tests/capi_prevsnapshot.rs)
 * compares its environments "prev-commit", "copy-compact",
 * "prepared-commit", "prepared-rollback", "rollback-prev" and
 * "rollback-prev-commit" byte for byte with C 1.0's,
 * and audits them. The second form runs while the test has <env> open
 * (after two commits: a=1, then b=2): a writer gets MDB_ENV_BUSY, a
 * read-only handle reads the previous snapshot.
 *
 * Prints "capi_prevsnapshot: N checks, F failures" and exits 1 on
 * failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
/* mkdir; its Windows form (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"
#ifdef LMDB_RUST
/* librlmdb: the format switch (rlmdb_env_set_format). */
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
#define SAME(got, want) do { const char *g_ = (got), *w_ = (want); checks++; \
	if (strcmp(g_, w_)) { failures++; \
		fprintf(stderr, "%s:%d: got \"%s\", expected \"%s\"\n", __FILE__, __LINE__, g_, w_); \
	} } while (0)

/* What C 1.0 gives (c10) and what rlmdb gives (rust). */
#ifdef LMDB_RUST
#define BY_LIB(c10, rust) (rust)
#else
#define BY_LIB(c10, rust) (c10)
#endif

/* A trace line, compared between the builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

static char scratch[1024];
/* The format asked for: LMDB 0.9 (librlmdb only) or 1.0. */
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
 * make 1.0 environments, librlmdb is asked for the format. */
static MDB_env *create_env(void)
{
	MDB_env *env;
	OK(mdb_env_create(&env));
#ifdef LMDB_LIBRLMDB
	OK(rlmdb_env_set_format(env, v09 ? RLMDB_FORMAT_09 : RLMDB_FORMAT_10));
#endif
	OK(mdb_env_set_mapsize(env, 8u << 20));
	return env;
}

/* Opens <scratch>/<name> with `flags`; returns mdb_env_open's result and
 * the handle in *envp (closed on failure). */
static int open_env(const char *name, unsigned flags, MDB_env **envp)
{
	char path[1200];
	MDB_env *env = create_env();
	int rc;
	env_path(path, sizeof(path), name);
	rc = mdb_env_open(env, path, flags | MDB_NOTLS, 0664);
	if (rc) {
		mdb_env_close(env);
		env = NULL;
	}
	*envp = env;
	return rc;
}

static void put(MDB_env *env, const char *k, const char *d)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key = sv(k), data = sv(d);
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_put(txn, dbi, &key, &data, 0));
	OK(mdb_txn_commit(txn));
}

/* "k=v k=v @txnid" of the main DB in a read transaction. */
static const char *contents(MDB_env *env)
{
	static char buf[512];
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_cursor *mc;
	MDB_val k, v;
	size_t n = 0;
	buf[0] = 0;
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_cursor_open(txn, dbi, &mc));
	while (!mdb_cursor_get(mc, &k, &v, MDB_NEXT) && n + 64 < sizeof(buf))
		n += (size_t)snprintf(buf + n, sizeof(buf) - n, "%.*s=%.*s ",
			(int)k.mv_size, (char *)k.mv_data, (int)v.mv_size, (char *)v.mv_data);
	snprintf(buf + n, sizeof(buf) - n, "@%zu", (size_t)mdb_txn_id(txn));
	mdb_cursor_close(mc);
	mdb_txn_abort(txn);
	return buf;
}

static unsigned prev_flag(MDB_env *env)
{
	unsigned flags;
	OK(mdb_env_get_flags(env, &flags));
	return flags & MDB_PREVSNAPSHOT;
}

static size_t last_txnid(MDB_env *env)
{
	MDB_envinfo info;
	OK(mdb_env_info(env, &info));
	return info.me_last_txnid;
}

/* A new environment <name> after two commits: a=1 (txn 1), b=2 (txn 2). */
static void two_commits(const char *name)
{
	MDB_env *env;
	OK(open_env(name, 0, &env));
	put(env, "a", "1");
	put(env, "b", "2");
	mdb_env_close(env);
}

/* ---------------------------------------------------------------- */

static void test_flag(void)
{
	MDB_env *env;

	/* Not changeable: mdb_env_set_flags refuses it, before and after
	 * the open. */
	env = create_env();
	RC(mdb_env_set_flags(env, MDB_PREVSNAPSHOT, 1), EINVAL);
	mdb_env_close(env);

	/* A new environment: both meta pages hold txnid 0. */
	OK(open_env("new", MDB_PREVSNAPSHOT, &env));
	T("new: flag %#x, last txnid %zu, %s", prev_flag(env), last_txnid(env), contents(env));
	RC(mdb_env_set_flags(env, MDB_PREVSNAPSHOT, 1), EINVAL);
	RC(mdb_env_set_flags(env, MDB_PREVSNAPSHOT, 0), EINVAL);
	put(env, "a", "1");
	T("new: after a commit: flag %#x, last txnid %zu", prev_flag(env), last_txnid(env));
	/* C 1.0 published the previous snapshot's txnid at the commit
	 * (row 67): its readers do not see it, its next writer overwrites
	 * it. */
	SAME(contents(env), BY_LIB("@0", "a=1 @1"));
	put(env, "b", "2");
	SAME(contents(env), BY_LIB("b=2 @1", "a=1 b=2 @2"));
	mdb_env_close(env);

	/* Read-only on a missing environment: as without the flag. */
	RC(open_env("missing", MDB_PREVSNAPSHOT | MDB_RDONLY, &env), ENOENT);
}

static void test_previous_snapshot(void)
{
	MDB_env *env;
	MDB_txn *txn, *child;
	MDB_dbi dbi;
	MDB_stat st;
	MDB_val key = sv("x"), data = sv("y");

	two_commits("prev-commit");
	OK(open_env("prev-commit", MDB_PREVSNAPSHOT, &env));
	OK(mdb_env_stat(env, &st));
	T("prev: flag %#x, last txnid %zu, entries %zu, %s", prev_flag(env), last_txnid(env),
		(size_t)st.ms_entries, contents(env));

	/* An empty commit, an abort, a nested commit and a commit of a
	 * read-only transaction keep the flag. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	T("prev: write txn id %zu", (size_t)mdb_txn_id(txn));
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_put(txn, dbi, &key, &data, 0));
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_txn_commit(txn));
	T("prev: after an empty commit and an abort: flag %#x, %s", prev_flag(env), contents(env));

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_txn_begin(env, txn, 0, &child));
	key = sv("c");
	data = sv("3");
	OK(mdb_put(child, dbi, &key, &data, 0));
	OK(mdb_txn_commit(child));
	T("prev: after a nested commit: flag %#x", prev_flag(env));
	OK(mdb_txn_commit(txn));
	T("prev: after the commit: flag %#x, last txnid %zu", prev_flag(env), last_txnid(env));
	SAME(contents(env), BY_LIB("a=1 @1", "a=1 c=3 @2"));
	mdb_env_close(env);

	/* Read-only: the previous snapshot until the handle is closed. */
	OK(open_env("prev-commit", MDB_PREVSNAPSHOT | MDB_RDONLY, &env));
	T("prev: read-only: flag %#x, last txnid %zu, %s", prev_flag(env), last_txnid(env),
		contents(env));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	mdb_txn_reset(txn);
	OK(mdb_txn_renew(txn));
	T("prev: read-only: renewed txn id %zu", (size_t)mdb_txn_id(txn));
	mdb_txn_abort(txn);
	mdb_env_close(env);
	OK(open_env("prev-commit", MDB_RDONLY, &env));
	T("prev: %s", contents(env));
	mdb_env_close(env);

	/* The second commit after the open: C 1.0 published the previous
	 * snapshot's txnid at the first one, so the second one began from
	 * that snapshot and overwrote the first (row 67). */
	two_commits("prev-two");
	OK(open_env("prev-two", MDB_PREVSNAPSHOT, &env));
	put(env, "c", "3");
	put(env, "d", "4");
	SAME(contents(env), BY_LIB("a=1 d=4 @2", "a=1 c=3 d=4 @3"));
	mdb_env_close(env);
	OK(open_env("prev-two", MDB_RDONLY, &env));
	SAME(contents(env), BY_LIB("a=1 d=4 @2", "a=1 c=3 d=4 @3"));
	mdb_env_close(env);
}

static void test_copy(void)
{
	char path[1200];
	MDB_env *env, *copy;

	two_commits("copy-src");
	OK(open_env("copy-src", MDB_PREVSNAPSHOT | MDB_RDONLY, &env));
	env_path(path, sizeof(path), "copy-compact");
	OK(mdb_env_copy2(env, path, MDB_CP_COMPACT));
	env_path(path, sizeof(path), "copy-plain");
	OK(mdb_env_copy2(env, path, 0));
	mdb_env_close(env);

	OK(open_env("copy-compact", MDB_RDONLY, &copy));
	T("copy: compacting: %s", contents(copy));
	mdb_env_close(copy);
	/* C 1.0's plain copy holds both meta pages as they are, and the
	 * pages up to the previous snapshot's last one: its newer meta page
	 * names pages it lacks (row 69). rlmdb's is a copy of the previous
	 * snapshot. */
	RC(open_env("copy-plain", MDB_RDONLY, &copy), BY_LIB(MDB_PAGE_NOTFOUND, 0));
	if (copy) {
		SAME(contents(copy), "a=1 @1");
		mdb_env_close(copy);
	}
	OK(open_env("copy-plain", MDB_RDONLY | MDB_PREVSNAPSHOT, &copy));
	T("copy: plain, previous snapshot: %s", contents(copy));
	mdb_env_close(copy);
}

static void test_nolock(void)
{
	MDB_env *env, *other;

	two_commits("nolock");
	OK(open_env("nolock", MDB_PREVSNAPSHOT | MDB_NOLOCK, &env));
	T("nolock: flag %#x, %s", prev_flag(env), contents(env));
	/* rlmdb's MDB_NOLOCK handles of one file share their readers and
	 * writer lock; a handle on the previous snapshot has the file to
	 * itself until its first commit. C coordinates nothing. */
	RC(open_env("nolock", MDB_NOLOCK, &other), BY_LIB(0, EBUSY));
	if (other)
		mdb_env_close(other);
	put(env, "c", "3");
	T("nolock: after the commit: flag %#x, %s", prev_flag(env), contents(env));
	OK(open_env("nolock", MDB_NOLOCK, &other));
	T("nolock: other handle: %s", contents(other));
	put(env, "d", "4");
	T("nolock: other handle: %s", contents(other));
	mdb_env_close(other);
	mdb_env_close(env);
}

/* Two-phase commit: the commit of a prepared transaction is the first
 * commit (C reaches the same code from it); a rollback while the flag is
 * set. */
static void test_two_phase(void)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	int i;
	MDB_val key = sv("c"), data = sv("3");

	/* A prepared commit on the previous snapshot. */
	two_commits("prepared-commit");
	OK(open_env("prepared-commit", MDB_PREVSNAPSHOT, &env));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_put(txn, dbi, &key, &data, 0));
	OK(mdb_txn_prepare(txn));
	T("2pc: prepared txn %zu: flag %#x", (size_t)mdb_txn_id(txn), prev_flag(env));
	OK(mdb_txn_commit(txn));
	T("2pc: after the prepared commit: flag %#x, last txnid %zu", prev_flag(env),
		last_txnid(env));
	/* C 1.0 published the previous snapshot's txnid (row 67). */
	SAME(contents(env), BY_LIB("a=1 @1", "a=1 c=3 @2"));
	mdb_env_close(env);
	OK(open_env("prepared-commit", MDB_RDONLY, &env));
	T("2pc: %s", contents(env));
	mdb_env_close(env);

	/* The same, then the prepared commit rolled back. */
	two_commits("prepared-rollback");
	OK(open_env("prepared-rollback", MDB_PREVSNAPSHOT, &env));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_put(txn, dbi, &key, &data, 0));
	OK(mdb_txn_prepare(txn));
	OK(mdb_txn_commit(txn));
	OK(mdb_env_rollback(env, 2));
	T("2pc: rolled back: flag %#x, last txnid %zu, %s", prev_flag(env), last_txnid(env),
		contents(env));
	mdb_env_close(env);

	/* A rollback while the flag is set. The environment reports the
	 * previous snapshot's txnid; the newest commit, one more, is the one
	 * to roll back, and the reported one cannot be. Both write C's page
	 * and publish the previous txnid. C keeps the flag, whose rule then
	 * picks the page just written: mdb_env_info and mdb_env_stat report
	 * txnid 0 and an empty main DB (its read transactions follow the lock
	 * file and still read the previous snapshot); rlmdb ends the flag, as
	 * the first commit does (row 71). A second rollback fails in both. */
	for (i = 0; i < 2; i++) {
		const char *name = i ? "rollback-prev-commit" : "rollback-prev";
		MDB_stat st;
		size_t reported;
		two_commits(name);
		OK(open_env(name, MDB_PREVSNAPSHOT, &env));
		reported = last_txnid(env);
		RC(mdb_env_rollback(env, reported), MDB_CANT_ROLLBACK);
		OK(mdb_env_rollback(env, reported + 1));
		OK(mdb_env_stat(env, &st));
		CHECK(prev_flag(env) == BY_LIB(MDB_PREVSNAPSHOT, 0));
		CHECK(last_txnid(env) == BY_LIB(0, 1));
		CHECK(st.ms_entries == BY_LIB(0, 1));
		T("2pc: rolled back %zu with the flag: %s", reported + 1, contents(env));
		RC(mdb_env_rollback(env, reported), MDB_CANT_ROLLBACK);
		RC(mdb_env_rollback(env, reported + 1), MDB_CANT_ROLLBACK);
		if (i) {
			put(env, "c", "3");
			T("2pc: after the commit: flag %#x, last txnid %zu", prev_flag(env),
				last_txnid(env));
			/* C 1.0 published the previous snapshot's txnid (row 67). */
			SAME(contents(env), BY_LIB("a=1 @1", "a=1 c=3 @2"));
		}
		mdb_env_close(env);
	}
}

/* While another process has <env> open: no writer on the previous
 * snapshot; a read-only handle reads it. */
static void test_shared(const char *path)
{
	MDB_env *env;
	MDB_txn *txn;

	env = create_env();
	RC(mdb_env_open(env, path, MDB_PREVSNAPSHOT | MDB_NOTLS, 0664), MDB_ENV_BUSY);
	T("shared: writer: busy");
	mdb_env_close(env);

	env = create_env();
	OK(mdb_env_open(env, path, MDB_PREVSNAPSHOT | MDB_RDONLY | MDB_NOTLS, 0664));
	T("shared: read-only: flag %#x, last txnid %zu, %s", prev_flag(env), last_txnid(env),
		contents(env));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	T("shared: read-only: txn id %zu", (size_t)mdb_txn_id(txn));
	mdb_txn_abort(txn);
	mdb_env_close(env);
}

int main(int argc, char **argv)
{
	if (!(argc == 3 || (argc == 5 && !strcmp(argv[3], "shared")))
		|| (strcmp(argv[2], "0.9") && strcmp(argv[2], "1.0"))) {
		fprintf(stderr, "usage: capi_prevsnapshot <scratch-dir> <0.9|1.0> [shared <env>]\n");
		return 2;
	}
	snprintf(scratch, sizeof(scratch), "%s", argv[1]);
	v09 = !strcmp(argv[2], "0.9");
	if (argc == 5) {
		test_shared(argv[4]);
	} else {
		test_flag();
		test_previous_snapshot();
		test_copy();
		test_nolock();
		test_two_phase();
	}
	printf("capi_prevsnapshot: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
