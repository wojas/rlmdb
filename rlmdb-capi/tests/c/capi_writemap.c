/* MDB_WRITEMAP through the C API.
 *
 *   capi_writemap <scratch-dir> <format: 0.9|1.0>
 *
 * Built against C LMDB 0.9 and C LMDB 1.0 (the oracles, each with its
 * lmdb.h) and against the Rust libraries (-DLMDB_RUST): librlmdb with
 * 0.9's lmdb.h and with its own headers, and the LMDB 1.0 stub. rlmdb
 * does not use a writable map, but what the flag changes through the API
 * is C's: mdb_env_get_flags reports it, mdb_env_set_flags cannot change
 * it, a read-only open drops it, and nested write transactions are
 * refused (MDB_BAD_TXN, C 0.9's code; C 1.0 returns EINVAL there, and
 * also allows read-only children, which rlmdb does not have: C10_RC()
 * marks what only C 1.0 returns).
 *
 * Every build prints the same trace ("T ..." lines). The same writes run
 * in <scratch>/writemap (with MDB_WRITEMAP) and in <scratch>/plain
 * (without), each followed by a compacting copy (<name>-copy); the test
 * compares the files the Rust builds leave in the two. Their trace lines
 * start with the environment's name.
 *
 * On OpenBSD the Rust builds refuse MDB_WRITEMAP for an open for writing
 * (ENOTSUP; rlmdb/src/os/mod.rs): there the writemap environment's
 * trace is the single line "T writemap: open refused" (WRITEMAP_REFUSED).
 * A read-only open with the flag works everywhere (it drops the flag).
 *
 * Prints "capi_writemap: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* mkdir; its Windows form (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"
#ifdef LMDB_RUST
/* The format switch (rlmdb_env_set_format). */
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

/* C LMDB 1.0 itself (its lmdb.h, not a Rust build): C10_RC(c10, other)
 * is c10 there, other everywhere else (C 0.9 and the Rust builds, whose
 * return codes stay 0.9's). */
#if MDB_VERSION_MAJOR >= 1 && !defined(LMDB_RUST)
#define C10_RC(c10, other) (c10)
#else
#define C10_RC(c10, other) (other)
#endif

/* rlmdb refuses MDB_WRITEMAP for an open for writing on OpenBSD, which
 * keeps a file's maps apart from its descriptors; C accepts it. */
#if defined(LMDB_RUST) && defined(__OpenBSD__)
#define WRITEMAP_REFUSED 1
#else
#define WRITEMAP_REFUSED 0
#endif

/* A trace line, compared between the builds. */
#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)
/* A traced return code. */
#define TRC(what, expr) T("%s: %d", what, (expr))
/* The same in writes(), after the environment's name. */
#define NTRC(what, expr) T("%s: %s: %d", name, what, (expr))

/* The flags this program sets; mdb_env_get_flags is traced through it. */
#define FLAG_MASK (MDB_WRITEMAP | MDB_RDONLY | MDB_NOTLS | MDB_MAPASYNC | MDB_NOSYNC)

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

/* The environment <scratch>/<name>, opened with flags; NULL where the
 * open is refused (WRITEMAP_REFUSED). */
static MDB_env *open_env(const char *name, unsigned int flags)
{
	char path[1200];
	MDB_env *env = create_env();
	int rc;
	env_path(path, sizeof(path), name);
	OK(mdb_env_set_mapsize(env, 4u << 20));
	OK(mdb_env_set_maxdbs(env, 4));
	rc = mdb_env_open(env, path, flags | MDB_NOTLS, 0664);
#if WRITEMAP_REFUSED
	if ((flags & (MDB_WRITEMAP | MDB_RDONLY)) == MDB_WRITEMAP) {
		RC(rc, ENOTSUP);
		mdb_env_close(env);
		return NULL;
	}
#endif
	OK(rc);
	return env;
}

static unsigned int env_flags(MDB_env *env)
{
	unsigned int flags = 0;
	OK(mdb_env_get_flags(env, &flags));
	return flags & FLAG_MASK;
}

static void put(MDB_txn *txn, MDB_dbi dbi, const char *k, const char *d)
{
	MDB_val key = sv(k), data = sv(d);
	OK(mdb_put(txn, dbi, &key, &data, 0));
}

/* The value of k in txn (at most 20 bytes of it), "-" if there is none
 * (in one of four buffers, so that a trace line can show several). */
static const char *get(MDB_txn *txn, MDB_dbi dbi, const char *k)
{
	static char bufs[4][32];
	static int next;
	char *buf = bufs[next++ % 4];
	MDB_val key = sv(k), data;
	if (mdb_get(txn, dbi, &key, &data))
		return "-";
	snprintf(buf, sizeof(bufs[0]), "%.*s", (int)(data.mv_size < 20 ? data.mv_size : 20),
		(char *)data.mv_data);
	return buf;
}

/* With MDB_WRITEMAP: a nested write transaction is refused (MDB_BAD_TXN;
 * C 1.0: EINVAL), a read-only one too (C 1.0 allows it), and neither
 * changes the parent. Without it: nothing (the writes must stay the
 * same in both environments). */
static void try_children(MDB_txn *txn, int writemap)
{
	MDB_txn *child = NULL;
	int rc;
	if (!writemap)
		return;
	RC(mdb_txn_begin(mdb_txn_env(txn), txn, 0, &child), C10_RC(EINVAL, MDB_BAD_TXN));
	rc = mdb_txn_begin(mdb_txn_env(txn), txn, MDB_RDONLY, &child);
	RC(rc, C10_RC(0, MDB_BAD_TXN));
	if (rc == 0)
		mdb_txn_abort(child);
}

/* The writes, the same with and without MDB_WRITEMAP. */
static void writes(const char *name, unsigned int flags)
{
	int writemap = (flags & MDB_WRITEMAP) != 0;
	MDB_env *env = open_env(name, flags);
	MDB_txn *txn, *child, *reader;
	MDB_dbi dbi, named;
	MDB_val key, data;
	MDB_envinfo info;
	MDB_cursor *cur;
	static char big[10000];
	char path[1200], k[16];
	int i, n;

	if (!env) {
		T("%s: open refused", name);
		return;
	}
	T("%s: flags after open: %#x", name, env_flags(env));

	/* MDB_WRITEMAP is not one of the flags that can change. */
	NTRC("set_flags(WRITEMAP, 0)", mdb_env_set_flags(env, MDB_WRITEMAP, 0));
	NTRC("set_flags(WRITEMAP, 1)", mdb_env_set_flags(env, MDB_WRITEMAP, 1));
	T("%s: flags: %#x", name, env_flags(env));
	/* MDB_MAPASYNC can, and syncs work with it. */
	NTRC("set_flags(MAPASYNC, 1)", mdb_env_set_flags(env, MDB_MAPASYNC, 1));
	T("%s: flags: %#x", name, env_flags(env));
	NTRC("sync(0)", mdb_env_sync(env, 0));
	NTRC("sync(1)", mdb_env_sync(env, 1));
	NTRC("set_flags(MAPASYNC, 0)", mdb_env_set_flags(env, MDB_MAPASYNC, 0));

	memset(big, 'b', sizeof(big));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	try_children(txn, writemap);
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	OK(mdb_dbi_open(txn, "named", MDB_CREATE | MDB_DUPSORT, &named));
	for (i = 0; i < 300; i++) {
		snprintf(k, sizeof(k), "k%04d", i);
		key = sv(k);
		data.mv_size = i % 50 ? 10 + i % 30 : sizeof(big);
		data.mv_data = big;
		OK(mdb_put(txn, dbi, &key, &data, 0));
	}
	put(txn, named, "d", "1");
	put(txn, named, "d", "2");
	try_children(txn, writemap);
	/* The parent is still usable. */
	put(txn, dbi, "after", "children");
	T("%s: in txn: after=%s k0001=%s", name, get(txn, dbi, "after"), get(txn, dbi, "k0001"));
	OK(mdb_txn_commit(txn));

	/* A read-only parent: EINVAL, as without MDB_WRITEMAP. */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &reader));
	NTRC("child of a reader", mdb_txn_begin(env, reader, 0, &child));
	T("%s: reader: after=%s", name, get(reader, dbi, "after"));
	mdb_txn_abort(reader);

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	for (i = 0; i < 300; i += 3) {
		snprintf(k, sizeof(k), "k%04d", i);
		key = sv(k);
		OK(mdb_del(txn, dbi, &key, NULL));
	}
	try_children(txn, writemap);
	put(txn, named, "d", "3");
	OK(mdb_txn_commit(txn));

	/* An aborted write changes nothing. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	put(txn, dbi, "aborted", "x");
	try_children(txn, writemap);
	mdb_txn_abort(txn);

	NTRC("set_mapsize(8 MiB)", mdb_env_set_mapsize(env, 8u << 20));
	OK(mdb_env_info(env, &info));
	T("%s: mapsize %lu", name, (unsigned long)info.me_mapsize);

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	put(txn, dbi, "grown", "yes");
	try_children(txn, writemap);
	OK(mdb_txn_commit(txn));

	{
		char copy[64];
		snprintf(copy, sizeof(copy), "%s-copy", name);
		env_path(path, sizeof(path), copy);
	}
	NTRC("copy2(COMPACT)", mdb_env_copy2(env, path, MDB_CP_COMPACT));
	mdb_env_close(env);

	/* A read-only open drops MDB_WRITEMAP (also where an open for
	 * writing with it is refused). */
	env = open_env(name, MDB_WRITEMAP | MDB_RDONLY);
	T("%s: read-only: flags %#x", name, env_flags(env));
	RC(mdb_txn_begin(env, NULL, 0, &txn), C10_RC(MDB_IS_READONLY, EACCES));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &reader));
	OK(mdb_dbi_open(reader, NULL, 0, &dbi));
	OK(mdb_cursor_open(reader, dbi, &cur));
	n = 0;
	while (mdb_cursor_get(cur, &key, &data, n ? MDB_NEXT : MDB_FIRST) == 0)
		n++;
	mdb_cursor_close(cur);
	T("%s: read-only: %d keys, grown=%s, aborted=%s", name, n,
		get(reader, dbi, "grown"), get(reader, dbi, "aborted"));
	mdb_txn_abort(reader);
	mdb_env_close(env);
}

int main(int argc, char **argv)
{
	int major, minor, patch;
	MDB_env *env;
	unsigned int flags = 0;
	if (argc != 3 || (strcmp(argv[2], "0.9") && strcmp(argv[2], "1.0"))) {
		fprintf(stderr, "usage: capi_writemap <scratch-dir> <0.9|1.0>\n");
		return 2;
	}
	snprintf(scratch, sizeof(scratch), "%s", argv[1]);
	v09 = !strcmp(argv[2], "0.9");
	mdb_version(&major, &minor, &patch);
	printf("library: LMDB %d.%d.%d\n", major, minor, patch);

	/* Before the open, too, only the changeable flags can be set. */
	OK(mdb_env_create(&env));
	TRC("set_flags(WRITEMAP, 1) before open", mdb_env_set_flags(env, MDB_WRITEMAP, 1));
	OK(mdb_env_get_flags(env, &flags));
	T("flags before open: %#x", flags & FLAG_MASK);
	mdb_env_close(env);

	writes("writemap", MDB_WRITEMAP);
	writes("plain", 0);

	printf("capi_writemap: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
