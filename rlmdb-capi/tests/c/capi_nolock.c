/* Read transactions in an MDB_NOLOCK environment, through the C API.
 *
 *   capi_nolock <scratch-dir>
 *
 * Built like capi_write.c: against the C library (the oracle) and against
 * the Rust library with -DLMDB_RUST. Without a lock file C tracks no
 * readers: a write transaction of the same process reuses pages that a
 * live read transaction still reads (lmdb.h makes avoiding that the
 * caller's job), so values the read transaction returned change under it.
 * The Rust library tracks the read transactions of the process in memory
 * (KNOWN_C_BUGS.md #35) and keeps their pages. Each build checks what its
 * library does there (NOLOCK_READERS() below); everything else, including
 * what mdb_env_info, mdb_reader_list and mdb_reader_check report, must be
 * the same, and the trace ("T ..." lines) must be identical.
 *
 * The second part opens the environment twice in the process, both
 * handles with MDB_NOLOCK (as C allows): the Rust library makes them share
 * one reader table and writer lock, so a reader of one handle keeps its
 * pages while the other commits, and their writers are serialised (C
 * coordinates nothing between them, nor within one handle).
 *
 * Prints "capi_nolock: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* pthreads, mkdir, unistd.h, dlopen; their Windows forms (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

#define RC(expr, expected) do { int rc_ = (expr), ex_ = (expected); checks++; \
	if (rc_ != ex_) { failures++; \
		fprintf(stderr, "%s:%d: %s returned %d (%s), expected %d (%s)\n", \
			__FILE__, __LINE__, #expr, rc_, mdb_strerror(rc_), ex_, mdb_strerror(ex_)); \
	} } while (0)

#define OK(expr) RC(expr, 0)

/* The documented difference: without a lock file C reuses pages of live
 * read transactions (c_cond holds), the Rust library does not (rust_cond). */
#ifdef LMDB_RUST
#define NOLOCK_READERS(c_cond, rust_cond) CHECK(rust_cond)
#else
#define NOLOCK_READERS(c_cond, rust_cond) CHECK(c_cond)
#endif

/* A trace line, compared between the C and Rust builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

#define KEYS 300
/* Keys whose values the long-lived reader holds. */
#define HELD 24

struct buf { char s[4096]; size_t n; };

static int collect(const char *msg, void *ctx)
{
	struct buf *b = ctx;
	size_t len = strlen(msg);
	if (b->n + len < sizeof(b->s)) {
		memcpy(b->s + b->n, msg, len + 1);
		b->n += len;
	}
	return 0;
}

static size_t psize;

/* Value of key i in round `round`: every seventh an overflow run. */
static size_t value(unsigned i, unsigned round, unsigned char *buf)
{
	size_t len = i % 7 == 0 ? 2 * psize : 100;
	memset(buf, (unsigned char)(i ^ round), len);
	memcpy(buf, &round, sizeof(round));
	return len;
}

static unsigned char *valbuf;

/* Rewrites every key in txn, which frees every page of the tree. */
static void rewrite(MDB_txn *txn, MDB_dbi dbi, unsigned round, unsigned from, unsigned to)
{
	unsigned i;
	for (i = from; i < to; i++) {
		unsigned char kb[4] = { i >> 24, i >> 16, i >> 8, i };
		MDB_val k, d;
		k.mv_size = sizeof(kb);
		k.mv_data = kb;
		d.mv_size = value(i, round, valbuf);
		d.mv_data = valbuf;
		OK(mdb_put(txn, dbi, &k, &d, 0));
	}
}

/* One commit of `round`: half of the keys in the transaction, the other
 * half in a nested one. */
static void commit_round(MDB_env *env, MDB_dbi dbi, unsigned round)
{
	MDB_txn *txn, *child;
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	rewrite(txn, dbi, round, 0, KEYS / 2);
	OK(mdb_txn_begin(env, txn, 0, &child));
	rewrite(child, dbi, round, KEYS / 2, KEYS);
	OK(mdb_txn_commit(child));
	OK(mdb_txn_commit(txn));
}

/* What the reader-table functions report: as C without a lock table. */
static void check_reader_table(MDB_env *env, const char *when)
{
	MDB_envinfo info;
	struct buf b;
	int dead = -1;
	OK(mdb_env_info(env, &info));
	CHECK(info.me_numreaders == 0);
	CHECK(info.me_maxreaders == 126);
	memset(&b, 0, sizeof(b));
	OK(mdb_reader_list(env, collect, &b));
	CHECK(!strcmp(b.s, "(no reader locks)\n"));
	b.s[strcspn(b.s, "\n")] = 0;
	OK(mdb_reader_check(env, &dead));
	CHECK(dead == 0);
	T("%s: numreaders %u maxreaders %u list %s", when, info.me_numreaders,
		info.me_maxreaders, b.s);
}

/* Opens the environment at `path` with MDB_NOLOCK, and its main DB (a DBI
 * is per handle: C installs the comparator in mdb_dbi_open). */
static MDB_env *open_nolock(const char *path, MDB_dbi *dbi)
{
	MDB_env *env;
	MDB_txn *txn;
	OK(mdb_env_create(&env));
	OK(mdb_env_set_mapsize(env, 256 << 20));
	OK(mdb_env_open(env, path, MDB_NOLOCK | MDB_NOSYNC, 0664));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, dbi));
	OK(mdb_txn_commit(txn));
	return env;
}

/* Reads the values a reader holds (HELD of them, every 13th key). */
static void hold(MDB_txn *r, MDB_dbi dbi, MDB_val *held, unsigned char **copies)
{
	unsigned i;
	for (i = 0; i < HELD; i++) {
		unsigned key = i * 13;
		unsigned char kb[4] = { key >> 24, key >> 16, key >> 8, key };
		MDB_val k;
		k.mv_size = sizeof(kb);
		k.mv_data = kb;
		OK(mdb_get(r, dbi, &k, &held[i]));
		copies[i] = malloc(held[i].mv_size);
		memcpy(copies[i], held[i].mv_data, held[i].mv_size);
	}
}

/* How many of the held values changed; frees the copies. */
static unsigned changed_since(MDB_val *held, unsigned char **copies)
{
	unsigned i, changed = 0;
	for (i = 0; i < HELD; i++) {
		changed += memcmp(held[i].mv_data, copies[i], held[i].mv_size) != 0;
		free(copies[i]);
	}
	return changed;
}

#ifdef LMDB_RUST
/* A write transaction of handle A that waits before it ends. */
struct writer {
	MDB_env *env;
	pthread_mutex_t m;
	pthread_cond_t c;
	int began, ended;
};

static void *write_and_wait(void *arg)
{
	struct writer *w = arg;
	MDB_txn *txn;
	OK(mdb_txn_begin(w->env, NULL, 0, &txn));
	pthread_mutex_lock(&w->m);
	w->began = 1;
	pthread_cond_broadcast(&w->c);
	pthread_mutex_unlock(&w->m);
	usleep(200 * 1000);
	pthread_mutex_lock(&w->m);
	w->ended = 1;
	pthread_mutex_unlock(&w->m);
	mdb_txn_abort(txn);
	return NULL;
}
#endif

/* Two MDB_NOLOCK handles of one environment in this process. */
static void two_handles(const char *scratch)
{
	char path[1024];
	MDB_env *a, *b, *c;
	MDB_txn *txn, *r;
	MDB_dbi dbi, dbi_a, dbi_c;
	MDB_stat st;
	MDB_val held[HELD];
	unsigned char *copies[HELD];
	unsigned round, changed;

	snprintf(path, sizeof(path), "%s/two", scratch);
	mkdir(path, 0775);
	a = open_nolock(path, &dbi_a);
	b = open_nolock(path, &dbi);
	for (round = 0; round < 3; round++)
		commit_round(b, dbi, round);

	/* A reader in A holds values while B rewrites every page. */
	OK(mdb_txn_begin(a, NULL, MDB_RDONLY, &r));
	hold(r, dbi_a, held, copies);
	for (round = 3; round < 15; round++)
		commit_round(b, dbi, round);
	changed = changed_since(held, copies);
	NOLOCK_READERS(changed > 0, changed == 0);
	mdb_txn_abort(r);

	/* A write transaction of B waits for the one of A. */
#ifdef LMDB_RUST
	{
		struct writer w = { a, PTHREAD_MUTEX_INITIALIZER, PTHREAD_COND_INITIALIZER, 0, 0 };
		pthread_t tid;
		CHECK(pthread_create(&tid, NULL, write_and_wait, &w) == 0);
		pthread_mutex_lock(&w.m);
		while (!w.began)
			pthread_cond_wait(&w.c, &w.m);
		pthread_mutex_unlock(&w.m);
		OK(mdb_txn_begin(b, NULL, 0, &txn));
		pthread_mutex_lock(&w.m);
		CHECK(w.ended);
		pthread_mutex_unlock(&w.m);
		mdb_txn_abort(txn);
		CHECK(pthread_join(tid, NULL) == 0);
	}
#else
	checks++; /* C leaves serialising writers to the caller. */
#endif

	/* Closing A while a reader of B is live; a new handle C writes. */
	OK(mdb_txn_begin(b, NULL, MDB_RDONLY, &r));
	hold(r, dbi, held, copies);
	mdb_env_close(a);
	c = open_nolock(path, &dbi_c);
	for (round = 15; round < 27; round++)
		commit_round(c, dbi_c, round);
	changed = changed_since(held, copies);
	NOLOCK_READERS(changed > 0, changed == 0);
	mdb_txn_abort(r);

	OK(mdb_txn_begin(c, NULL, MDB_RDONLY, &r));
	OK(mdb_stat(r, dbi_c, &st));
	T("two handles: entries %zu depth %u txnid %zu", st.ms_entries, st.ms_depth,
		mdb_txn_id(r));
	mdb_txn_abort(r);
	mdb_env_close(b);
	mdb_env_close(c);
}

int main(int argc, char **argv)
{
	char path[1024];
	MDB_env *env;
	MDB_txn *txn, *r;
	MDB_dbi dbi;
	MDB_stat st;
	MDB_envinfo info;
	MDB_val held[HELD];
	unsigned char *copies[HELD];
	unsigned i, round, changed;
	size_t last_pinned, last_free;
	struct stat sb;

	if (argc != 2) {
		fprintf(stderr, "usage: capi_nolock <scratch-dir>\n");
		return 2;
	}
	setvbuf(stdout, NULL, _IOLBF, 0);
	snprintf(path, sizeof(path), "%s/nolock", argv[1]);
	mkdir(path, 0775);

	OK(mdb_env_create(&env));
	OK(mdb_env_set_mapsize(env, 256 << 20));
	OK(mdb_env_open(env, path, MDB_NOLOCK | MDB_NOSYNC, 0664));
	snprintf(path, sizeof(path), "%s/nolock/lock.mdb", argv[1]);
	CHECK(stat(path, &sb) == -1 && errno == ENOENT);
	OK(mdb_env_stat(env, &st));
	psize = st.ms_psize;
	valbuf = malloc(2 * psize);
	check_reader_table(env, "empty");

	/* (A commit that changes nothing does not write a meta page.) */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_txn_commit(txn));
	for (round = 0; round < 3; round++)
		commit_round(env, dbi, round);

	/* A reader holds values while a dozen commits rewrite every page. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	CHECK(mdb_txn_id(r) == 3);
	for (i = 0; i < HELD; i++) {
		unsigned key = i * 13;
		unsigned char kb[4] = { key >> 24, key >> 16, key >> 8, key };
		MDB_val k;
		k.mv_size = sizeof(kb);
		k.mv_data = kb;
		OK(mdb_get(r, dbi, &k, &held[i]));
		copies[i] = malloc(held[i].mv_size);
		memcpy(copies[i], held[i].mv_data, held[i].mv_size);
	}
	check_reader_table(env, "reader live");
	for (round = 3; round < 15; round++)
		commit_round(env, dbi, round);
	check_reader_table(env, "after commits");
	changed = 0;
	for (i = 0; i < HELD; i++)
		changed += memcmp(held[i].mv_data, copies[i], held[i].mv_size) != 0;
	NOLOCK_READERS(changed > 0, changed == 0);
	CHECK(mdb_txn_id(r) == 3);
	OK(mdb_env_info(env, &info));
	last_pinned = info.me_last_pgno;

	/* Reset and renew: the newest snapshot. */
	mdb_txn_reset(r);
	OK(mdb_txn_renew(r));
	CHECK(mdb_txn_id(r) == 15);
	for (i = 0; i < HELD; i++) {
		unsigned key = i * 13;
		unsigned char kb[4] = { key >> 24, key >> 16, key >> 8, key };
		MDB_val k, d;
		k.mv_size = sizeof(kb);
		k.mv_data = kb;
		OK(mdb_get(r, dbi, &k, &d));
		CHECK(d.mv_size == value(key, 14, valbuf) && !memcmp(d.mv_data, valbuf, d.mv_size));
		free(copies[i]);
	}
	mdb_txn_abort(r);

	/* With no reader left, the writers reuse the pages it kept: the file
	 * stops growing (in the Rust build too, so aborting unregistered it). */
	commit_round(env, dbi, 15);
	OK(mdb_env_info(env, &info));
	last_free = info.me_last_pgno;
	for (round = 16; round < 20; round++)
		commit_round(env, dbi, round);
	OK(mdb_env_info(env, &info));
	CHECK(info.me_last_pgno == last_free);
	CHECK(last_free >= last_pinned);
	check_reader_table(env, "end");

	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_stat(r, dbi, &st));
	T("entries %zu depth %u txnid %zu", st.ms_entries, st.ms_depth, mdb_txn_id(r));
	mdb_txn_abort(r);
	mdb_env_close(env);

	two_handles(argv[1]);
	free(valbuf);

	printf("capi_nolock: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
