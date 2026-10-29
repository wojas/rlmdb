/* cpeer_misc.c - create, stat, readers, reader-check, copy, nested-test and gen. */
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "cpeer.h"

/* ---- create ------------------------------------------------------------- */

int
cmd_create(struct opts *o)
{
	MDB_env *env;
	MDB_stat st;
	MDB_envinfo info;

	o->mapsize = parse_u64(o->pos[1], "mapsize");
	o->maxdbs = parse_u32(o->pos[2], "maxdbs");
	env = open_env(o, 0);
	CHECK(mdb_env_stat(env, &st));
	CHECK(mdb_env_info(env, &info));
	printf("created psize=%u mapsize=%zu maxkeysize=%d last_txnid=%zu\n", st.ms_psize,
		info.me_mapsize, mdb_env_get_maxkeysize(env), info.me_last_txnid);
	mdb_env_close(env);
	return 0;
}

/* ---- stat --------------------------------------------------------------- */

/* "db=<label> [flags=0x..] depth=.. branch_pages=.. leaf_pages=..
 * overflow_pages=.. entries=..". mdb_dbi_flags() rejects DBI 0 (the free DB),
 * so its line has no flags. */
static void
print_stat_line(MDB_txn *txn, MDB_dbi dbi, const char *label)
{
	MDB_stat st;
	unsigned int flags;

	CHECK(mdb_stat(txn, dbi, &st));
	printf("db=%s ", label);
	if (dbi != 0) {
		CHECK(mdb_dbi_flags(txn, dbi, &flags));
		printf("flags=0x%x ", flags);
	}
	printf("depth=%u branch_pages=%zu leaf_pages=%zu overflow_pages=%zu entries=%zu\n",
		st.ms_depth, st.ms_branch_pages, st.ms_leaf_pages, st.ms_overflow_pages,
		st.ms_entries);
}

int
cmd_stat(struct opts *o)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_stat st;
	MDB_envinfo info;
	MDB_cursor *cur;
	MDB_val key, data;
	MDB_dbi main;
	size_t free_pages = 0;
	int rc;

	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_env_stat(env, &st));
	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	CHECK(mdb_env_info(env, &info));
	printf("psize=%u\nmapsize=%zu\nlast_pgno=%zu\nlast_txnid=%zu\nmaxreaders=%u\n"
		"numreaders=%u\nmaxkeysize=%d\n", st.ms_psize, info.me_mapsize,
		info.me_last_pgno, info.me_last_txnid, info.me_maxreaders,
		info.me_numreaders, mdb_env_get_maxkeysize(env));

	/* The free DB (DBI 0): records are txnid -> [count, pgno...]. */
	CHECK(mdb_cursor_open(txn, 0, &cur));
	while ((rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT)) == 0) {
		size_t n;
		memcpy(&n, data.mv_data, sizeof(n));	/* data is only 2-byte aligned */
		free_pages += n;
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(free DB)");
	mdb_cursor_close(cur);
	printf("free_pages=%zu\n", free_pages);
	print_stat_line(txn, 0, "@free");

	/* Main DB, then every named DB it records (as mdb_stat -a). */
	CHECK(mdb_dbi_open(txn, NULL, 0, &main));
	print_stat_line(txn, main, "-");
	CHECK(mdb_cursor_open(txn, main, &cur));
	while ((rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_NODUP)) == 0) {
		MDB_dbi dbi;
		char *name;
		if (memchr(key.mv_data, '\0', key.mv_size))
			continue;
		name = malloc(key.mv_size + 1);
		memcpy(name, key.mv_data, key.mv_size);
		name[key.mv_size] = '\0';
		if (mdb_dbi_open(txn, name, 0, &dbi) == 0)
			print_stat_line(txn, dbi, name);
		free(name);
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(main)");
	mdb_cursor_close(cur);
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return 0;
}

/* ---- readers ------------------------------------------------------------ */

/* mdb_reader_list() callback: lines are "%10d %zx %zu\n" (pid, thread,
 * txnid) or "%10d %zx -\n" for a slot not in a transaction, after a header
 * line; "(no active readers)\n" if there are none. */
static int
reader_line(const char *msg, void *ctx)
{
	int pid;
	char tid[32], txnid[32];

	if (sscanf(msg, "%d %31s %31s", &pid, tid, txnid) == 3) {
		printf("reader pid=%d tid=0x%s txnid=%s\n", pid, tid, txnid);
		(*(int *)ctx)++;
	}
	return 0;
}

int
cmd_readers(struct opts *o)
{
	MDB_env *env;
	MDB_envinfo info;
	int n = 0;

	/* No transaction of our own, so our pid is not in the table. */
	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_env_info(env, &info));
	if (mdb_reader_list(env, reader_line, &n) < 0)
		die(EXIT_LMDB, "mdb_reader_list failed");
	printf("readers=%d numreaders=%u maxreaders=%u\n", n, info.me_numreaders,
		info.me_maxreaders);
	mdb_env_close(env);
	return 0;
}

/* ---- reader-check ------------------------------------------------------- */

/* mdb_reader_check(): clear the reader slots of dead processes. */
int
cmd_reader_check(struct opts *o)
{
	MDB_env *env;
	int dead = 0;

	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_reader_check(env, &dead));
	printf("reader-check cleared=%d\n", dead);
	mdb_env_close(env);
	return 0;
}

/* ---- copy --------------------------------------------------------------- */

int
cmd_copy(struct opts *o)
{
	const char *dest = o->pos[1];
	MDB_env *env;

	/* The destination directory must exist (as for mdb_copy); with
	 * --nosubdir `dest` is the new data file. The data file itself must not
	 * exist: mdb_env_copy2 opens it with O_EXCL. */
	if (!o->nosubdir && mkdir(dest, 0775) && errno != EEXIST)
		die(EXIT_LMDB, "mkdir %s: %s", dest, strerror(errno));
	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_env_copy2(env, dest, o->compact ? MDB_CP_COMPACT : 0));
	printf("copied compact=%d\n", o->compact);
	mdb_env_close(env);
	return 0;
}

/* ---- gen ---------------------------------------------------------------- */

/* Print the generated key and values of item i: test vectors for the Rust
 * implementation of the data set. */
int
cmd_gen(struct opts *o)
{
	struct dataset *ds = &o->ds;
	unsigned char kbuf[DS_MAX_KEY], *vbuf;
	const char *err;
	uint32_t i, d;
	size_t klen;

	ds->seed = parse_u32(o->pos[0], "seed");
	i = parse_u32(o->pos[1], "i");
	if (i == UINT32_MAX)
		die(EXIT_USAGE, "i must be < 2^32 - 1");
	ds->count = i + 1;
	if ((err = ds_finish(ds)))
		die(EXIT_USAGE, "%s", err);
	vbuf = malloc(ds->big > ds->valsize ? ds->big : ds->valsize);
	klen = ds_key(ds, i, kbuf);
	printf("key=");
	print_hex(stdout, kbuf, klen);
	printf("\nndups=%u\n", ds_ndups(ds, i));
	for (d = 0; d < ds_ndups(ds, i); d++) {
		size_t len = ds_vlen(ds, i, d);
		ds_value(ds, i, d, vbuf, len);
		printf("value%u=", d);
		print_hex(stdout, vbuf, len);
		putchar('\n');
	}
	free(vbuf);
	return 0;
}

/* ---- nested-test -------------------------------------------------------- */

/* Nested write transactions: child commits fold into the parent, child
 * aborts leave the parent untouched, at any depth, including overflow pages,
 * page splits, DB creation and mdb_drop. The final content of the DB
 * "nested" (checked by --check, in any peer) is:
 *   k1="v1" k2="v2" k3="v3" k5=<40000-byte pattern, seed 5>
 *   n%05u="val%05u" for odd u < 2000
 * and the DBs "nested2" and "nested3" do not exist. Running the write mode
 * again reaches the same state. */

#define NESTED_N	2000
#define BIG_LEN		40000

static unsigned char big[BIG_LEN];

static void
make_big(void)
{
	uint64_t state = 5, x = 0;
	size_t off;
	/* SplitMix64 words, little-endian */
	for (off = 0; off < BIG_LEN; off++) {
		if (off % 8 == 0)
			x = splitmix64(&state);
		big[off] = (unsigned char)(x >> (8 * (off % 8)));
	}
}

static void
put_s(MDB_txn *txn, MDB_dbi dbi, const char *k, const void *v, size_t vlen)
{
	MDB_val key, data;
	key.mv_data = (void *)k;
	key.mv_size = strlen(k);
	data.mv_data = (void *)v;
	data.mv_size = vlen;
	CHECK(mdb_put(txn, dbi, &key, &data, 0));
}

static void
put_str(MDB_txn *txn, MDB_dbi dbi, const char *k, const char *v)
{
	put_s(txn, dbi, k, v, strlen(v));
}

static void
del_s(MDB_txn *txn, MDB_dbi dbi, const char *k)
{
	MDB_val key;
	key.mv_data = (void *)k;
	key.mv_size = strlen(k);
	CHECK(mdb_del(txn, dbi, &key, NULL));
}

/* Expect k to hold v (vlen bytes), or to be absent if v is NULL. */
static void
expect(const char *where, MDB_txn *txn, MDB_dbi dbi, const char *k, const void *v, size_t vlen)
{
	MDB_val key, data;
	int rc;

	key.mv_data = (void *)k;
	key.mv_size = strlen(k);
	rc = mdb_get(txn, dbi, &key, &data);
	if (!v) {
		if (rc != MDB_NOTFOUND)
			die(EXIT_MISMATCH, "nested-test %s: %s should be absent (rc=%d)", where, k, rc);
		return;
	}
	if (rc == MDB_NOTFOUND)
		die(EXIT_MISMATCH, "nested-test %s: %s not found", where, k);
	check_rc(rc, "mdb_get");
	if (data.mv_size != vlen || memcmp(data.mv_data, v, vlen))
		die(EXIT_MISMATCH, "nested-test %s: %s has the wrong value (%zu bytes)", where, k,
			data.mv_size);
}

static void
expect_str(const char *where, MDB_txn *txn, MDB_dbi dbi, const char *k, const char *v)
{
	expect(where, txn, dbi, k, v, v ? strlen(v) : 0);
}

static void
expect_entries(const char *where, MDB_txn *txn, MDB_dbi dbi, size_t n)
{
	MDB_stat st;
	CHECK(mdb_stat(txn, dbi, &st));
	if (st.ms_entries != n)
		die(EXIT_MISMATCH, "nested-test %s: %zu entries, expected %zu", where,
			st.ms_entries, n);
}

static void
expect_no_db(MDB_env *env, const char *name)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	int rc;

	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	rc = mdb_dbi_open(txn, name, 0, &dbi);
	mdb_txn_abort(txn);
	if (rc != MDB_NOTFOUND)
		die(EXIT_MISMATCH, "nested-test: DB %s should not exist (rc=%d)", name, rc);
}

/* The final state, as listed above; walks the whole DB. */
static void
nested_check(MDB_env *env)
{
	static const char *const small[][2] = {
		{ "k1", "v1" }, { "k2", "v2" }, { "k3", "v3" }
	};
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_cursor *cur;
	MDB_val key, data;
	char kbuf[16], vbuf[16];
	int rc, n = 0, u = 1, k;

	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	rc = mdb_dbi_open(txn, "nested", 0, &dbi);
	if (rc == MDB_NOTFOUND)
		die(EXIT_MISMATCH, "nested-test: DB nested not found");
	check_rc(rc, "mdb_dbi_open(nested)");
	for (k = 0; k < 3; k++)
		expect_str("check", txn, dbi, small[k][0], small[k][1]);
	expect("check", txn, dbi, "k5", big, BIG_LEN);
	expect_entries("check", txn, dbi, 4 + NESTED_N / 2);

	/* Cursor order: k1 k2 k3 k5 n00001 n00003 ... */
	CHECK(mdb_cursor_open(txn, dbi, &cur));
	while ((rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT)) == 0) {
		const char *want;
		if (n < 4) {
			want = n < 3 ? small[n][0] : "k5";
		} else {
			snprintf(kbuf, sizeof(kbuf), "n%05d", u);
			snprintf(vbuf, sizeof(vbuf), "val%05d", u);
			if (data.mv_size != strlen(vbuf) || memcmp(data.mv_data, vbuf, data.mv_size))
				die(EXIT_MISMATCH, "nested-test check: %s has the wrong value", kbuf);
			want = kbuf;
			u += 2;
		}
		if (key.mv_size != strlen(want) || memcmp(key.mv_data, want, key.mv_size))
			die(EXIT_MISMATCH, "nested-test check: entry %d is %.*s, expected %s", n,
				(int)key.mv_size, (char *)key.mv_data, want);
		n++;
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get");
	mdb_cursor_close(cur);
	mdb_txn_abort(txn);
	expect_no_db(env, "nested2");
	expect_no_db(env, "nested3");
}

static void
nested_write(MDB_env *env)
{
	MDB_txn *t, *c, *g;
	MDB_dbi dbi, d2, d3;
	char k[16], v[16];
	int u;

	/* 1. A committed child is visible to the parent and is persisted. */
	t = begin_write(env);
	CHECK(mdb_dbi_open(t, "nested", MDB_CREATE, &dbi));
	put_str(t, dbi, "k1", "v1");
	CHECK(mdb_txn_begin(env, t, 0, &c));
	expect_str("1/child", c, dbi, "k1", "v1");
	put_str(c, dbi, "k2", "v2");
	CHECK(mdb_txn_commit(c));
	expect_str("1/parent", t, dbi, "k2", "v2");
	commit_write(t);

	/* 2. An aborted child changes nothing, including overwrites. */
	t = begin_write(env);
	put_str(t, dbi, "k3", "v3");
	CHECK(mdb_txn_begin(env, t, 0, &c));
	put_str(c, dbi, "k4", "v4");
	put_str(c, dbi, "k1", "v1b");
	expect_str("2/child", c, dbi, "k3", "v3");
	mdb_txn_abort(c);
	expect_str("2/parent", t, dbi, "k4", NULL);
	expect_str("2/parent", t, dbi, "k1", "v1");
	commit_write(t);

	/* 3. Overflow pages, and a committed grandchild inside an aborted child. */
	t = begin_write(env);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	put_s(c, dbi, "k5", big, BIG_LEN);
	CHECK(mdb_txn_commit(c));
	expect("3/parent", t, dbi, "k5", big, BIG_LEN);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	del_s(c, dbi, "k5");
	put_str(c, dbi, "k6", "v6");
	CHECK(mdb_txn_begin(env, c, 0, &g));
	put_str(g, dbi, "k7", "v7");
	del_s(g, dbi, "k2");
	expect_str("3/grandchild", g, dbi, "k5", NULL);
	CHECK(mdb_txn_commit(g));
	expect_str("3/child", c, dbi, "k7", "v7");
	expect_str("3/child", c, dbi, "k2", NULL);
	mdb_txn_abort(c);
	expect("3/parent", t, dbi, "k5", big, BIG_LEN);
	expect_str("3/parent", t, dbi, "k6", NULL);
	expect_str("3/parent", t, dbi, "k7", NULL);
	expect_str("3/parent", t, dbi, "k2", "v2");
	commit_write(t);

	/* 4. A DB created in a committed child of an aborted parent is gone. */
	t = begin_write(env);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	CHECK(mdb_dbi_open(c, "nested2", MDB_CREATE, &d2));
	put_str(c, d2, "x", "1");
	CHECK(mdb_txn_commit(c));
	expect_str("4/parent", t, d2, "x", "1");
	abort_write(t);
	expect_no_db(env, "nested2");

	/* 5. Page splits: an aborted child with many keys leaves no trace; a
	 *    committed child whose committed grandchild deletes half of them
	 *    leaves the other half. */
	t = begin_write(env);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	for (u = 0; u < NESTED_N; u++) {
		snprintf(k, sizeof(k), "n%05d", u);
		snprintf(v, sizeof(v), "val%05d", u);
		put_str(c, dbi, k, v);
	}
	mdb_txn_abort(c);
	expect_str("5/parent", t, dbi, "n00000", NULL);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	for (u = 0; u < NESTED_N; u++) {
		snprintf(k, sizeof(k), "n%05d", u);
		snprintf(v, sizeof(v), "val%05d", u);
		put_str(c, dbi, k, v);
	}
	CHECK(mdb_txn_begin(env, c, 0, &g));
	for (u = 0; u < NESTED_N; u += 2) {
		snprintf(k, sizeof(k), "n%05d", u);
		del_s(g, dbi, k);
	}
	CHECK(mdb_txn_commit(g));
	CHECK(mdb_txn_commit(c));
	expect_entries("5/parent", t, dbi, 4 + NESTED_N / 2);
	commit_write(t);

	/* 6. mdb_drop in children: emptying in an aborted child, deleting in a
	 *    committed one. */
	t = begin_write(env);
	CHECK(mdb_dbi_open(t, "nested3", MDB_CREATE, &d3));
	for (u = 0; u < 10; u++) {
		snprintf(k, sizeof(k), "z%d", u);
		put_str(t, d3, k, "z");
	}
	commit_write(t);
	t = begin_write(env);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	CHECK(mdb_drop(c, d3, 0));
	expect_entries("6/child", c, d3, 0);
	mdb_txn_abort(c);
	expect_entries("6/parent", t, d3, 10);
	CHECK(mdb_txn_begin(env, t, 0, &c));
	CHECK(mdb_drop(c, d3, 1));
	CHECK(mdb_txn_commit(c));
	commit_write(t);
	expect_no_db(env, "nested3");
}

int
cmd_nested_test(struct opts *o)
{
	MDB_env *env;

	make_big();
	env = open_env(o, o->check ? MDB_RDONLY : 0);
	if (!o->check)
		nested_write(env);
	nested_check(env);
	printf("nested ok mode=%s\n", o->check ? "check" : "write");
	mdb_env_close(env);
	return 0;
}
