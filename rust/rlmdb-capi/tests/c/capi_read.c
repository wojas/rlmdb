/* Exercises the read side of the LMDB C API and its error codes.
 *
 *   capi_read <fixture-env> <writable-env> <scratch-dir>
 *
 * <fixture-env> and <writable-env> are copies of the environment written by
 * fixture.c. The program is built twice: against the C library (the
 * oracle: every expectation below must hold for C LMDB 0.9.36) and against
 * the Rust library with -DLMDB_RUST. GAP() marks C behaviour the Rust
 * implementation does not have yet; in the Rust build it reports the gap,
 * and fails if the gap has closed (so the list stays accurate).
 *
 * Prints "capi_read: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
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

#ifdef LMDB_RUST
#define GAP(name, cond) do { if (cond) { failures++; \
	fprintf(stderr, "%s:%d: known gap \"%s\" now behaves like C: " \
		"update the test and rust/rlmdb-capi/README.md\n", __FILE__, __LINE__, name); \
	} else printf("known gap: %s\n", name); } while (0)
#else
#define GAP(name, cond) CHECK(cond)
#endif

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

static MDB_val uv(size_t *n)
{
	MDB_val v;
	v.mv_size = sizeof(*n);
	v.mv_data = n;
	return v;
}

static unsigned be32(const unsigned char *p)
{
	return (unsigned)p[0] << 24 | (unsigned)p[1] << 16 | (unsigned)p[2] << 8 | p[3];
}

struct buf { char s[8192]; size_t n; int calls; int ret; };

static int collect(const char *msg, void *ctx)
{
	struct buf *b = ctx;
	size_t len = strlen(msg);
	if (b->n + len < sizeof(b->s)) {
		memcpy(b->s + b->n, msg, len + 1);
		b->n += len;
	}
	b->calls++;
	return b->ret;
}

static void reader_list(MDB_env *env, struct buf *b)
{
	memset(b, 0, sizeof(*b));
	OK(mdb_reader_list(env, collect, b));
}

static void assert_cb(MDB_env *env, const char *msg)
{
	(void)env;
	fprintf(stderr, "assert callback: %s\n", msg);
}

static void test_version(void)
{
	int ma = -1, mi = -1, pa = -1;
	char *s = mdb_version(&ma, &mi, &pa), e[256];

	CHECK(ma == 0 && mi == 9 && pa == 36);
	CHECK(!strcmp(s, MDB_VERSION_STRING));
	CHECK(mdb_version(NULL, NULL, NULL) == s);
	CHECK(MDB_VERSION_FULL == MDB_VERINT(0, 9, 36));
	CHECK(!strcmp(mdb_strerror(0), "Successful return: 0"));
	CHECK(!strcmp(mdb_strerror(MDB_KEYEXIST), "MDB_KEYEXIST: Key/data pair already exists"));
	CHECK(!strcmp(mdb_strerror(MDB_NOTFOUND), "MDB_NOTFOUND: No matching key/data pair found"));
	CHECK(!strcmp(mdb_strerror(MDB_MAP_RESIZED),
		"MDB_MAP_RESIZED: Database contents grew beyond environment mapsize"));
	CHECK(!strcmp(mdb_strerror(MDB_BAD_DBI),
		"MDB_BAD_DBI: The specified DBI handle was closed/changed unexpectedly"));
	CHECK(!strcmp(mdb_strerror(MDB_LAST_ERRCODE + 1), "Invalid error code"));
	CHECK(!strcmp(mdb_strerror(-1), "Invalid error code"));
	snprintf(e, sizeof(e), "%s", strerror(ENOENT));
	CHECK(!strcmp(mdb_strerror(ENOENT), e));
}

static void test_unopened_env(const char *scratch)
{
	MDB_env *env;
	unsigned int flags = 99, r = 0;
	const char *p = "x";
	int fd = 0, x = 0, dead = 5;
	struct buf b;
	char path[1024];

	OK(mdb_env_create(&env));
	OK(mdb_env_get_flags(env, &flags));
	CHECK(flags == 0);
	OK(mdb_env_set_flags(env, MDB_NOSYNC | MDB_NOMETASYNC, 1));
	OK(mdb_env_get_flags(env, &flags));
	CHECK(flags == (MDB_NOSYNC | MDB_NOMETASYNC));
	OK(mdb_env_set_flags(env, MDB_NOSYNC, 0));
	OK(mdb_env_get_flags(env, &flags));
	CHECK(flags == MDB_NOMETASYNC);
	RC(mdb_env_set_flags(env, MDB_RDONLY, 1), EINVAL);
	RC(mdb_env_get_flags(env, NULL), EINVAL);
	RC(mdb_env_get_flags(NULL, &flags), EINVAL);
	OK(mdb_env_get_path(env, &p));
	CHECK(p == NULL);
	RC(mdb_env_get_path(NULL, &p), EINVAL);
	RC(mdb_env_get_path(env, NULL), EINVAL);
	OK(mdb_env_get_fd(env, &fd));
	CHECK(fd == -1);
	RC(mdb_env_get_fd(env, NULL), EINVAL);
	OK(mdb_env_get_maxreaders(env, &r));
	CHECK(r == 126);
	RC(mdb_env_set_maxreaders(env, 0), EINVAL);
	OK(mdb_env_set_maxreaders(env, 200));
	OK(mdb_env_get_maxreaders(env, &r));
	CHECK(r == 200);
	RC(mdb_env_get_maxreaders(NULL, &r), EINVAL);
	RC(mdb_env_get_maxreaders(env, NULL), EINVAL);
	OK(mdb_env_set_maxdbs(env, 3));
	OK(mdb_env_set_mapsize(env, 1 << 20));
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	OK(mdb_env_set_userctx(env, &x));
	CHECK(mdb_env_get_userctx(env) == &x);
	RC(mdb_env_set_userctx(NULL, &x), EINVAL);
	CHECK(mdb_env_get_userctx(NULL) == NULL);
	OK(mdb_env_set_assert(env, assert_cb));
	OK(mdb_env_set_assert(env, NULL));
	RC(mdb_env_set_assert(NULL, assert_cb), EINVAL);
	/* No lock table yet. */
	reader_list(env, &b);
	CHECK(!strcmp(b.s, "(no reader locks)\n"));
	RC(mdb_reader_list(NULL, collect, &b), -1);
	RC(mdb_reader_list(env, NULL, &b), -1);
	OK(mdb_reader_check(env, &dead));
	CHECK(dead == 0);
	RC(mdb_reader_check(NULL, &dead), EINVAL);
	mdb_env_close(env);
	mdb_env_close(NULL);

	/* Failed opens; after a failure only mdb_env_close is allowed. */
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, scratch, 0x02, 0664), EINVAL);
	mdb_env_close(env);
	snprintf(path, sizeof(path), "%s/does-not-exist", scratch);
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, path, MDB_RDONLY, 0664), ENOENT);
	mdb_env_close(env);
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, path, 0, 0664), ENOENT);
	mdb_env_close(env);
	snprintf(path, sizeof(path), "%s/empty", scratch);
	mkdir(path, 0775);
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, path, MDB_RDONLY, 0664), ENOENT);
	mdb_env_close(env);
	snprintf(path, sizeof(path), "%s/garbage", scratch);
	mkdir(path, 0775);
	{
		char file[1100], junk[4096];
		FILE *f;
		snprintf(file, sizeof(file), "%s/data.mdb", path);
		memset(junk, 'x', sizeof(junk));
		f = fopen(file, "wb");
		CHECK(f != NULL);
		if (f) {
			fwrite(junk, 1, sizeof(junk), f);
			fclose(f);
		}
	}
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, path, MDB_RDONLY, 0664), MDB_INVALID);
	mdb_env_close(env);
}

struct thread_arg { MDB_txn *txn; MDB_dbi dbi; int rc; MDB_val data; };

static void *get_in_thread(void *p)
{
	struct thread_arg *a = p;
	MDB_val k = sv("b");
	a->rc = mdb_get(a->txn, a->dbi, &k, &a->data);
	return NULL;
}

static void test_reads(MDB_env *env, const char *path)
{
	MDB_txn *txn, *txn2, *t;
	MDB_dbi plain, dups, fixed, intkey, numeric, revkeys, rdup, extra, main_, d2;
	MDB_cursor *cur, *dc, *fc, *c2;
	MDB_val k, d, d0, prev, m[2];
	MDB_stat st;
	MDB_envinfo info;
	unsigned int flags;
	const char *p;
	char expect[128];
	size_t cnt, n, i, ik, ik2, first_page, items, txnid;
	int fd, rc, sorted;
	unsigned last, sentinel;
	struct stat sb;
	struct buf b;
	struct thread_arg ta;
	pthread_t th;

	/* --- the open environment --- */
	RC(mdb_env_open(env, path, MDB_RDONLY, 0664), EINVAL);
	OK(mdb_env_get_flags(env, &flags));
	CHECK(flags == (MDB_NOSYNC | MDB_RDONLY | MDB_NOTLS));
	RC(mdb_env_set_maxdbs(env, 9), EINVAL);
	RC(mdb_env_set_maxreaders(env, 10), EINVAL);
	OK(mdb_env_get_path(env, &p));
	CHECK(p && !strcmp(p, path));
	OK(mdb_env_get_fd(env, &fd));
	CHECK(fd >= 0 && fstat(fd, &sb) == 0 && sb.st_size > 0);
	OK(mdb_env_get_maxreaders(env, &flags));
	CHECK(flags == 200);
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	OK(mdb_env_stat(env, &st));
	CHECK(st.ms_entries == 9);
	CHECK(st.ms_psize >= 4096 && st.ms_depth >= 1);
	RC(mdb_env_stat(env, NULL), EINVAL);
	RC(mdb_env_stat(NULL, &st), EINVAL);
	OK(mdb_env_info(env, &info));
	CHECK(info.me_mapaddr == NULL);
	CHECK(info.me_mapsize == 64UL << 20);
	CHECK(info.me_maxreaders == 200);
	CHECK(info.me_last_txnid == 1);
	CHECK(info.me_numreaders == 0);
	CHECK(info.me_last_pgno > 10);
	RC(mdb_env_info(env, NULL), EINVAL);
	RC(mdb_env_sync(env, 1), EACCES);
	OK(mdb_env_set_flags(env, MDB_NOMETASYNC, 1));
	OK(mdb_env_get_flags(env, &flags));
	CHECK(flags & MDB_NOMETASYNC);
	OK(mdb_env_set_flags(env, MDB_NOMETASYNC, 0));
	RC(mdb_env_set_flags(env, MDB_NOTLS, 1), EINVAL);

	/* --- handles used by the other transactions: opened (and comparators
	 * installed) in a read transaction that commits, the order lmdb.h
	 * documents; C publishes such handles only at commit --- */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_dbi_open(txn, "plain", 0, &plain));
	OK(mdb_dbi_open(txn, "dups", 0, &dups));
	OK(mdb_dbi_open(txn, "fixed", 0, &fixed));
	OK(mdb_dbi_open(txn, "intkey", 0, &intkey));
	OK(mdb_dbi_open(txn, "numeric", 0, &numeric));
	OK(mdb_set_compare(txn, numeric, numcmp));
	OK(mdb_dbi_open(txn, "rdup", 0, &rdup));
	OK(mdb_set_dupsort(txn, rdup, revcmp));
	OK(mdb_txn_commit(txn));

	/* --- transactions --- */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	CHECK(mdb_txn_env(txn) == env);
	CHECK(mdb_txn_id(txn) == info.me_last_txnid);
	RC(mdb_txn_begin(env, NULL, 0, &t), EACCES);
	RC(mdb_txn_begin(env, txn, MDB_RDONLY, &t), EINVAL);
	CHECK(mdb_txn_id(NULL) == 0);
	CHECK(mdb_txn_env(NULL) == NULL);
	RC(mdb_txn_commit(NULL), EINVAL);
	mdb_txn_abort(NULL);
	mdb_txn_reset(NULL);
	RC(mdb_txn_renew(NULL), EINVAL);
	RC(mdb_txn_renew(txn), EINVAL);
	OK(mdb_env_info(env, &info));
	CHECK(info.me_numreaders >= 1);

	/* --- database handles --- */
	OK(mdb_dbi_open(txn, "plain", 0, &d2));
	CHECK(d2 == plain);
	OK(mdb_dbi_open(txn, "plain", MDB_CREATE, &d2));
	CHECK(d2 == plain);
	OK(mdb_dbi_open(txn, NULL, 0, &main_));
	CHECK(main_ == 1);
	RC(mdb_dbi_open(txn, "nope", 0, &d2), MDB_NOTFOUND);
	RC(mdb_dbi_open(txn, "nope", MDB_CREATE, &d2), EACCES);
	RC(mdb_dbi_open(txn, "plain", 0x1000000, &d2), EINVAL);
	RC(mdb_dbi_open(txn, "notadb", 0, &d2), MDB_INCOMPATIBLE);
	OK(mdb_dbi_open(txn, "dups", 0, &d2));
	CHECK(d2 == dups);
	OK(mdb_dbi_flags(txn, plain, &flags));
	CHECK(flags == 0);
	OK(mdb_dbi_flags(txn, dups, &flags));
	CHECK(flags == MDB_DUPSORT);
	OK(mdb_dbi_flags(txn, fixed, &flags));
	CHECK(flags == (MDB_DUPSORT | MDB_DUPFIXED));
	OK(mdb_dbi_flags(txn, intkey, &flags));
	CHECK(flags == MDB_INTEGERKEY);
	OK(mdb_dbi_flags(txn, main_, &flags));
	CHECK(flags == 0);
	RC(mdb_dbi_flags(txn, 0, &flags), EINVAL);
	RC(mdb_dbi_flags(txn, 99, &flags), EINVAL);
	OK(mdb_stat(txn, plain, &st));
	CHECK(st.ms_entries == 101 && st.ms_overflow_pages == 4);
	OK(mdb_stat(txn, dups, &st));
	CHECK(st.ms_entries == 604);
	OK(mdb_stat(txn, fixed, &st));
	CHECK(st.ms_entries == 10006);
	OK(mdb_stat(txn, intkey, &st));
	CHECK(st.ms_entries == 50);
	OK(mdb_stat(txn, 0, &st));
	RC(mdb_stat(txn, 99, &st), EINVAL);
	RC(mdb_stat(txn, plain, NULL), EINVAL);
	RC(mdb_stat(NULL, plain, &st), EINVAL);

	/* --- mdb_get --- */
	k = sv("key042");
	p = k.mv_data;
	OK(mdb_get(txn, plain, &k, &d));
	CHECK(veq(d, "val042"));
	CHECK(k.mv_data == p);
	OK(mdb_get(txn, plain, &k, &d0));
	CHECK(d0.mv_data == d.mv_data);	/* zero copy: both point into the map */
	k = sv("key100");
	RC(mdb_get(txn, plain, &k, &d), MDB_NOTFOUND);
	k.mv_size = 0;
	RC(mdb_get(txn, plain, &k, &d), MDB_BAD_VALSIZE);
	k = sv("key001");
	RC(mdb_get(txn, plain, NULL, &d), EINVAL);
	RC(mdb_get(txn, plain, &k, NULL), EINVAL);
	RC(mdb_get(txn, 0, &k, &d), EINVAL);
	RC(mdb_get(txn, 99, &k, &d), EINVAL);
	RC(mdb_get(NULL, plain, &k, &d), EINVAL);
	k = sv("big");
	OK(mdb_get(txn, plain, &k, &d));
	CHECK(d.mv_size == 3 * st.ms_psize + 100);
	for (i = 0, rc = 1; i < d.mv_size; i++)
		rc &= ((unsigned char *)d.mv_data)[i] == (unsigned char)(i * 7);
	CHECK(rc);
	k = sv("b");
	OK(mdb_get(txn, dups, &k, &d));
	CHECK(veq(d, "b1"));
	ik = 7;
	k = uv(&ik);
	OK(mdb_get(txn, intkey, &k, &d));
	CHECK(veq(d, "i7"));

	/* --- a cursor on a plain database --- */
	OK(mdb_cursor_open(txn, plain, &cur));
	CHECK(mdb_cursor_txn(cur) == txn);
	CHECK(mdb_cursor_dbi(cur) == plain);
	CHECK(mdb_cursor_txn(NULL) == NULL);
	RC(mdb_cursor_open(txn, 99, &c2), EINVAL);
	RC(mdb_cursor_open(txn, plain, NULL), EINVAL);
	RC(mdb_cursor_open(NULL, plain, &c2), EINVAL);
	RC(mdb_cursor_get(cur, &k, &d, MDB_GET_CURRENT), EINVAL);
	n = 0;
	sorted = 1;
	prev.mv_size = 0;
	while ((rc = mdb_cursor_get(cur, &k, &d, MDB_NEXT)) == 0) {
		if (n++ > 0) {
			size_t l = prev.mv_size < k.mv_size ? prev.mv_size : k.mv_size;
			int c = memcmp(prev.mv_data, k.mv_data, l);
			sorted &= c < 0 || (c == 0 && prev.mv_size < k.mv_size);
		}
		prev = k;
	}
	RC(rc, MDB_NOTFOUND);
	CHECK(n == 101 && sorted);
	RC(mdb_cursor_get(cur, &k, &d, MDB_NEXT), MDB_NOTFOUND);
	OK(mdb_cursor_get(cur, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "key099") && veq(d, "val099"));
	OK(mdb_cursor_get(cur, &k, &d, MDB_FIRST));
	CHECK(veq(k, "big"));
	RC(mdb_cursor_get(cur, &k, &d, MDB_PREV), MDB_NOTFOUND);
	OK(mdb_cursor_get(cur, &k, &d, MDB_LAST));
	CHECK(veq(k, "key099") && veq(d, "val099"));
	OK(mdb_cursor_get(cur, &k, &d, MDB_PREV));
	CHECK(veq(k, "key098"));
	k = sv("key050");
	p = k.mv_data;
	OK(mdb_cursor_get(cur, &k, &d, MDB_SET));
	CHECK(k.mv_data == p && veq(d, "val050"));	/* MDB_SET leaves the key alone */
	OK(mdb_cursor_get(cur, &k, &d, MDB_SET_KEY));
	CHECK(k.mv_data != p && veq(k, "key050") && veq(d, "val050"));
	k = sv("key0505");
	OK(mdb_cursor_get(cur, &k, &d, MDB_SET_RANGE));
	CHECK(veq(k, "key051") && veq(d, "val051"));
	k = sv("key0505");
	RC(mdb_cursor_get(cur, &k, &d, MDB_SET), MDB_NOTFOUND);
	k = sv("zzz");
	RC(mdb_cursor_get(cur, &k, &d, MDB_SET_RANGE), MDB_NOTFOUND);
	k.mv_size = 0;
	RC(mdb_cursor_get(cur, &k, &d, MDB_SET), MDB_BAD_VALSIZE);
	RC(mdb_cursor_get(cur, NULL, &d, MDB_SET), EINVAL);
	k = sv("key001");
	d = sv("val001");
	RC(mdb_cursor_get(cur, &k, &d, MDB_GET_BOTH), MDB_INCOMPATIBLE);
	RC(mdb_cursor_get(cur, &k, NULL, MDB_GET_BOTH), EINVAL);
	OK(mdb_cursor_get(cur, &k, &d, MDB_FIRST));
	RC(mdb_cursor_get(cur, &k, &d, MDB_GET_MULTIPLE), MDB_INCOMPATIBLE);
	RC(mdb_cursor_get(cur, &k, &d, MDB_NEXT_MULTIPLE), MDB_INCOMPATIBLE);
	RC(mdb_cursor_get(cur, &k, &d, MDB_FIRST_DUP), MDB_INCOMPATIBLE);
	RC(mdb_cursor_get(cur, &k, NULL, MDB_FIRST_DUP), EINVAL);
	RC(mdb_cursor_count(cur, &cnt), MDB_INCOMPATIBLE);
	RC(mdb_cursor_count(cur, NULL), EINVAL);
	RC(mdb_cursor_count(NULL, &cnt), EINVAL);
	RC(mdb_cursor_get(cur, &k, &d, 99), EINVAL);
	RC(mdb_cursor_get(NULL, &k, &d, MDB_FIRST), EINVAL);
	OK(mdb_cursor_get(cur, NULL, NULL, MDB_NEXT));
	OK(mdb_cursor_get(cur, &k, NULL, MDB_NEXT));
	CHECK(veq(k, "key001"));

	/* --- MDB_DUPSORT --- */
	OK(mdb_cursor_open(txn, dups, &dc));
	RC(mdb_cursor_count(dc, &cnt), EINVAL);
	k = sv("a");
	OK(mdb_cursor_get(dc, &k, &d, MDB_SET));
	CHECK(veq(d, "only"));
	OK(mdb_cursor_count(dc, &cnt));
	CHECK(cnt == 1);
	/* a key without duplicates: MDB_FIRST_DUP stores the key */
	k = sv("sentinel");
	OK(mdb_cursor_get(dc, &k, &d, MDB_FIRST_DUP));
	CHECK(veq(k, "a") && veq(d, "only"));
	k = sv("b");
	OK(mdb_cursor_get(dc, &k, &d, MDB_SET));
	CHECK(veq(d, "b1"));
	OK(mdb_cursor_count(dc, &cnt));
	CHECK(cnt == 3);
	OK(mdb_cursor_get(dc, &k, &d, MDB_NEXT_DUP));
	CHECK(veq(k, "b") && veq(d, "b2"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_NEXT_DUP));
	CHECK(veq(d, "b3"));
	RC(mdb_cursor_get(dc, &k, &d, MDB_NEXT_DUP), MDB_NOTFOUND);
	OK(mdb_cursor_get(dc, &k, &d, MDB_GET_CURRENT));
	CHECK(veq(k, "b") && veq(d, "b3"));
	/* key untouched on a key with duplicates */
	k = sv("sentinel");
	OK(mdb_cursor_get(dc, &k, &d, MDB_FIRST_DUP));
	CHECK(veq(d, "b1") && veq(k, "sentinel"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_LAST_DUP));
	CHECK(veq(d, "b3") && veq(k, "sentinel"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_PREV_DUP));
	CHECK(veq(d, "b2"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_NEXT_NODUP));
	CHECK(veq(k, "c") && veq(d, "v0000"));
	OK(mdb_cursor_count(dc, &cnt));
	CHECK(cnt == 600);
	OK(mdb_cursor_get(dc, &k, &d, MDB_LAST_DUP));
	CHECK(veq(d, "v0599"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_PREV_NODUP));
	CHECK(veq(k, "b") && veq(d, "b3"));
	k = sv("c");
	d = sv("v0300");
	p = k.mv_data;
	OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH));
	CHECK(k.mv_data == p && veq(d, "v0300"));
	d = sv("v0300x");
	RC(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH), MDB_NOTFOUND);
	OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH_RANGE));
	CHECK(k.mv_data == p && veq(d, "v0301"));
	d = sv("zzz");
	RC(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH_RANGE), MDB_NOTFOUND);
	k = sv("a");
	d = sv("only");
	OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH));
	d = sv("onlz");
	RC(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH), MDB_NOTFOUND);
	d = sv("onl");
	OK(mdb_cursor_get(dc, &k, &d, MDB_GET_BOTH_RANGE));
	CHECK(veq(d, "only"));
	OK(mdb_cursor_get(dc, &k, &d, MDB_LAST));
	CHECK(veq(k, "c") && veq(d, "v0599"));
	RC(mdb_cursor_get(dc, &k, &d, MDB_GET_MULTIPLE), MDB_INCOMPATIBLE);
	for (n = 0, rc = mdb_cursor_get(dc, &k, &d, MDB_FIRST); rc == 0;
		 rc = mdb_cursor_get(dc, &k, &d, MDB_NEXT))
		n++;
	CHECK(n == 604);
	for (n = 0, rc = mdb_cursor_get(dc, &k, &d, MDB_FIRST); rc == 0;
		 rc = mdb_cursor_get(dc, &k, &d, MDB_NEXT_NODUP))
		n++;
	CHECK(n == 3);

	/* --- MDB_DUPFIXED --- */
	OK(mdb_cursor_open(txn, fixed, &fc));
	RC(mdb_cursor_get(fc, &k, &d, MDB_GET_MULTIPLE), EINVAL);
	RC(mdb_cursor_get(fc, &k, &d, MDB_PREV_MULTIPLE), MDB_NOTFOUND);
	k = sv("k1");
	OK(mdb_cursor_get(fc, &k, &d, MDB_SET));
	CHECK(d.mv_size == 4 && be32(d.mv_data) == 0);
	p = k.mv_data;
	OK(mdb_cursor_get(fc, &k, &d, MDB_GET_MULTIPLE));
	CHECK(k.mv_data == p);
	CHECK(d.mv_size % 4 == 0 && d.mv_size > 4);
	items = 0;
	rc = 1;
	first_page = d.mv_size / 4;
	for (i = 0; i < first_page; i++, items++)
		rc &= be32((unsigned char *)d.mv_data + 4 * i) == items;
	last = 0;
	while (mdb_cursor_get(fc, &k, &d, MDB_NEXT_MULTIPLE) == 0) {
		rc &= veq(k, "k1") && d.mv_size % 4 == 0;
		for (i = 0; i < d.mv_size / 4; i++, items++)
			rc &= be32((unsigned char *)d.mv_data + 4 * i) == items;
		last = (unsigned)(d.mv_size / 4);
	}
	CHECK(rc && items == 10000 && last > 0 && first_page < 10000);
	k = sv("sentinel");
	for (n = 0; mdb_cursor_get(fc, &k, &d, MDB_PREV_MULTIPLE) == 0;)
		n += d.mv_size / 4;
	CHECK(n + last == 10000);
	CHECK(veq(k, "sentinel"));	/* untouched on a positioned cursor */
	RC(mdb_cursor_get(fc, &k, NULL, MDB_GET_MULTIPLE), EINVAL);
	RC(mdb_cursor_get(fc, &k, NULL, MDB_NEXT_MULTIPLE), EINVAL);
	k = sv("k2");
	OK(mdb_cursor_get(fc, &k, &d, MDB_SET));
	CHECK(be32(d.mv_data) == 7);
	d.mv_size = 12345;
	d.mv_data = &sentinel;
	OK(mdb_cursor_get(fc, &k, &d, MDB_GET_MULTIPLE));
	CHECK(d.mv_size == 12345 && d.mv_data == &sentinel);	/* untouched */
	k = sv("k3");
	OK(mdb_cursor_get(fc, &k, &d, MDB_SET));
	OK(mdb_cursor_get(fc, &k, &d, MDB_GET_MULTIPLE));
	CHECK(d.mv_size == 20 && be32(d.mv_data) == 1 && be32((unsigned char *)d.mv_data + 16) == 5);
	OK(mdb_cursor_count(fc, &cnt));
	CHECK(cnt == 5);

	/* --- integer keys and the comparison functions --- */
	{
		MDB_cursor *ic;
		OK(mdb_cursor_open(txn, intkey, &ic));
		OK(mdb_cursor_get(ic, &k, &d, MDB_FIRST));
		CHECK(k.mv_size == sizeof(size_t) && *(size_t *)k.mv_data == 1);
		OK(mdb_cursor_get(ic, &k, &d, MDB_LAST));
		CHECK(*(size_t *)k.mv_data == 50 && veq(d, "i50"));
		mdb_cursor_close(ic);
	}
	ik = 256;
	ik2 = 1;
	k = uv(&ik);
	d = uv(&ik2);
	CHECK(mdb_cmp(txn, intkey, &k, &d) > 0);	/* native integers */
	CHECK(mdb_cmp(txn, 0, &k, &d) > 0);	/* the free DB's mdb_cmp_long */
	k = sv("9");
	d = sv("10");
	CHECK(mdb_cmp(txn, plain, &k, &d) > 0);
	CHECK(mdb_cmp(txn, plain, &k, &k) == 0);
	k = sv("b1");
	d = sv("b2");
	CHECK(mdb_dcmp(txn, dups, &k, &d) < 0);

	/* --- custom comparators (installed above) --- */
	RC(mdb_set_compare(txn, 0, numcmp), EINVAL);
	RC(mdb_set_compare(txn, 99, numcmp), EINVAL);
	RC(mdb_set_compare(NULL, plain, numcmp), EINVAL);
	RC(mdb_set_dupsort(txn, 99, revcmp), EINVAL);
	OK(mdb_set_relfunc(txn, plain, NULL));
	OK(mdb_set_relctx(txn, plain, &sentinel));
	RC(mdb_set_relfunc(txn, 99, NULL), EINVAL);
	RC(mdb_set_relctx(txn, 0, NULL), EINVAL);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn2));
	k = sv("57");
	OK(mdb_get(txn2, numeric, &k, &d));
	CHECK(veq(d, "57"));
	OK(mdb_cursor_open(txn2, numeric, &c2));
	OK(mdb_cursor_get(c2, &k, &d, MDB_FIRST));
	CHECK(veq(k, "1"));
	OK(mdb_cursor_get(c2, &k, &d, MDB_NEXT));
	CHECK(veq(k, "2"));
	k = sv("150");
	OK(mdb_cursor_get(c2, &k, &d, MDB_SET_RANGE));
	CHECK(veq(k, "150"));
	OK(mdb_cursor_get(c2, &k, &d, MDB_LAST));
	CHECK(veq(k, "200"));
	mdb_cursor_close(c2);
	k = sv("9");
	d = sv("10");
	CHECK(mdb_cmp(txn2, numeric, &k, &d) < 0);
	OK(mdb_cursor_open(txn2, rdup, &c2));
	k = sv("x");
	OK(mdb_cursor_get(c2, &k, &d, MDB_SET));
	CHECK(veq(d, "j"));
	d = sv("c");
	OK(mdb_cursor_get(c2, &k, &d, MDB_GET_BOTH));
	OK(mdb_cursor_get(c2, &k, &d, MDB_NEXT_DUP));
	CHECK(veq(d, "b"));
	mdb_cursor_close(c2);
	k = sv("a");
	d = sv("b");
	CHECK(mdb_dcmp(txn2, rdup, &k, &d) > 0);
	/* Installed in the transaction that opens the handle and used there. */
	OK(mdb_dbi_open(txn2, "revkeys", 0, &revkeys));
	OK(mdb_set_compare(txn2, revkeys, revcmp));
	k = sv("m");
	OK(mdb_get(txn2, revkeys, &k, &d));
	OK(mdb_txn_commit(txn2));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn2));
	OK(mdb_get(txn2, revkeys, &k, &d));
	CHECK(veq(d, "m"));

	/* --- MDB_DBS_FULL and mdb_dbi_close (maxdbs is 7) --- */
	RC(mdb_dbi_open(txn2, "extra", 0, &extra), MDB_DBS_FULL);
	mdb_cursor_close(cur);
	mdb_dbi_close(env, plain);
	mdb_dbi_close(env, plain);
	mdb_dbi_close(env, 0);
	mdb_dbi_close(env, 1000);
	mdb_txn_abort(txn2);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn2));
	k = sv("key001");
	RC(mdb_get(txn2, plain, &k, &d), EINVAL);
	OK(mdb_dbi_open(txn2, "extra", 0, &extra));
	k = sv("e");
	OK(mdb_get(txn2, extra, &k, &d));
	CHECK(veq(d, "1"));
	mdb_txn_abort(txn2);

	/* --- another thread may use a read transaction (MDB_NOTLS) --- */
	ta.txn = txn;
	ta.dbi = dups;
	ta.rc = -1;
	pthread_create(&th, NULL, get_in_thread, &ta);
	pthread_join(th, NULL);
	RC(ta.rc, 0);
	CHECK(veq(ta.data, "b1"));

	/* --- reader table --- */
	txnid = mdb_txn_id(txn);
	reader_list(env, &b);
	CHECK(!strncmp(b.s, "    pid     thread     txnid\n", 29));
	snprintf(expect, sizeof(expect), "%10d %zx %zu\n", (int)getpid(),
		(size_t)pthread_self(), txnid);
	CHECK(strstr(b.s, expect) != NULL);
	memset(&b, 0, sizeof(b));
	b.ret = -7;
	RC(mdb_reader_list(env, collect, &b), -7);
	CHECK(b.calls == 1);
	OK(mdb_reader_check(env, &rc));
	CHECK(rc == 0);

	/* --- writes in a read-only transaction: EACCES, after C's checks --- */
	k = sv("key001");
	d = sv("x");
	RC(mdb_put(txn, dups, &k, &d, 0), EACCES);
	RC(mdb_put(txn, dups, &k, &d, MDB_CURRENT), EINVAL);
	RC(mdb_put(txn, dups, NULL, &d, 0), EINVAL);
	RC(mdb_put(txn, 99, &k, &d, 0), EINVAL);
	RC(mdb_del(txn, dups, &k, NULL), EACCES);
	RC(mdb_del(txn, dups, NULL, NULL), EINVAL);
	RC(mdb_drop(txn, dups, 0), EACCES);
	RC(mdb_drop(txn, dups, 2), EINVAL);
	RC(mdb_cursor_put(dc, &k, &d, 0), EACCES);
	RC(mdb_cursor_put(dc, NULL, &d, 0), EINVAL);
	RC(mdb_cursor_del(dc, 0), EACCES);
	m[0].mv_size = 4;
	m[0].mv_data = &sentinel;
	m[1].mv_size = 0;
	m[1].mv_data = NULL;
	RC(mdb_cursor_put(fc, &k, m, MDB_MULTIPLE), EINVAL);
	m[1].mv_size = 3;
	RC(mdb_cursor_put(dc, &k, m, MDB_MULTIPLE), MDB_INCOMPATIBLE);
	CHECK(m[1].mv_size == 0);
	m[1].mv_size = 3;
	RC(mdb_cursor_put(fc, &k, m, MDB_MULTIPLE), EACCES);

	/* --- reset and renew --- */
	mdb_txn_reset(txn);
	mdb_txn_reset(txn);
	CHECK(mdb_txn_id(txn) == txnid);
	reader_list(env, &b);
	snprintf(expect, sizeof(expect), "%10d %zx -\n", (int)getpid(), (size_t)pthread_self());
	CHECK(strstr(b.s, expect) != NULL);
	k = sv("b");
	RC(mdb_get(txn, dups, &k, &d), EINVAL);
	RC(mdb_cursor_get(dc, &k, &d, MDB_FIRST), MDB_BAD_TXN);
	RC(mdb_cursor_count(dc, &cnt), MDB_BAD_TXN);
	RC(mdb_cursor_open(txn, dups, &c2), EINVAL);
	RC(mdb_dbi_open(txn, "dups", 0, &d2), MDB_BAD_TXN);
	RC(mdb_stat(txn, dups, &st), EINVAL);
	RC(mdb_dbi_flags(txn, dups, &flags), EINVAL);
	RC(mdb_cursor_renew(txn, dc), EINVAL);
	RC(mdb_put(txn, dups, &k, &d, 0), EINVAL);
	RC(mdb_cursor_put(dc, &k, &d, 0), EACCES);
	RC(mdb_set_compare(txn, dups, revcmp), EINVAL);
	CHECK(mdb_cursor_txn(dc) == txn);
	OK(mdb_txn_renew(txn));
	RC(mdb_txn_renew(txn), EINVAL);
	OK(mdb_cursor_renew(txn, dc));
	OK(mdb_cursor_get(dc, &k, &d, MDB_FIRST));
	CHECK(veq(k, "a") && veq(d, "only"));
	RC(mdb_cursor_renew(txn, NULL), EINVAL);
	RC(mdb_cursor_renew(NULL, dc), EINVAL);

	/* --- cursors outlive read-only transactions --- */
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	OK(mdb_cursor_renew(txn, dc));
	CHECK(mdb_cursor_txn(dc) == txn);
	OK(mdb_cursor_get(dc, &k, &d, MDB_LAST));
	CHECK(veq(k, "c") && veq(d, "v0599"));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn2));
	OK(mdb_cursor_renew(txn2, dc));	/* while its transaction is live */
	CHECK(mdb_cursor_txn(dc) == txn2);
	OK(mdb_cursor_get(dc, &k, &d, MDB_FIRST));
	CHECK(veq(k, "a"));
	OK(mdb_txn_commit(txn2));
	mdb_cursor_close(dc);
	mdb_cursor_close(NULL);
	OK(mdb_cursor_renew(txn, fc));
	OK(mdb_cursor_get(fc, &k, &d, MDB_FIRST));
	CHECK(veq(k, "k1"));
	/* Closing order: transaction first, cursor afterwards. */
	OK(mdb_txn_commit(txn));
	mdb_cursor_close(fc);
}

static void test_writable_env(const char *path, const char *scratch)
{
	MDB_env *env;
	MDB_txn *txn, *t;
	char dir[1024];
	int rc, fd;

	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 16));
	OK(mdb_env_open(env, path, 0, 0664));
	OK(mdb_env_sync(env, 1));
	OK(mdb_env_sync(env, 0));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	RC(mdb_txn_begin(env, txn, 0, &t), EINVAL);
	mdb_txn_abort(txn);
	OK(mdb_txn_begin(env, NULL, 0, &t));
	mdb_txn_abort(t);
	OK(mdb_env_set_mapsize(env, 128UL << 20));
	{
		MDB_envinfo info;
		OK(mdb_env_info(env, &info));
		CHECK(info.me_mapsize == 128UL << 20);
	}

	snprintf(dir, sizeof(dir), "%s/copy", scratch);
	mkdir(dir, 0775);
	rc = mdb_env_copy(env, dir);
	RC(rc, 0);
	snprintf(dir, sizeof(dir), "%s/copy-compact", scratch);
	mkdir(dir, 0775);
	rc = mdb_env_copy2(env, dir, MDB_CP_COMPACT);
	RC(rc, 0);
	snprintf(dir, sizeof(dir), "%s/copy.mdb", scratch);
	fd = open(dir, O_WRONLY | O_CREAT | O_TRUNC, 0664);
	CHECK(fd >= 0);
	rc = mdb_env_copyfd(env, fd);
	RC(rc, 0);
	rc = mdb_env_copyfd2(env, fd, 0);
	RC(rc, 0);
	close(fd);
	mdb_env_close(env);
}

int main(int argc, char **argv)
{
	MDB_env *env;

	if (argc != 4) {
		fprintf(stderr, "usage: capi_read <fixture-env> <writable-env> <scratch-dir>\n");
		return 2;
	}
	test_version();
	test_unopened_env(argv[3]);

	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 7));
	OK(mdb_env_set_mapsize(env, 64UL << 20));
	OK(mdb_env_set_maxreaders(env, 200));
	OK(mdb_env_set_flags(env, MDB_NOSYNC, 1));
	OK(mdb_env_open(env, argv[1], MDB_RDONLY | MDB_NOTLS, 0664));
	test_reads(env, argv[1]);
	mdb_env_close(env);

	test_writable_env(argv[2], argv[3]);

	printf("capi_read: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
