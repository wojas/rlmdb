/* Checks the data-file format support of the C API against the Rust
 * library: rlmdb.h's format functions and LMDB 1.0 constants, and LMDB 1.0
 * environments through the lmdb.h functions (C LMDB has no rlmdb.h, and
 * C 0.9 cannot open 1.0 files, so there is no C build; interop's
 * tests/capi10.rs has C 1.0 read and write what this library writes).
 *
 *   capi_format <scratch-dir>
 *
 * The program changes the process default format and the
 * RLMDB_DEFAULT_FORMAT variable; both are its own, so nothing else is
 * affected. Prints "capi_format: N checks, F failures" and exits 1 on
 * failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

#define RC(expr, expected) do { int rc_ = (expr), ex_ = (expected); checks++; \
	if (rc_ != ex_) { failures++; \
		fprintf(stderr, "%s:%d: %s returned %d (%s), expected %d (%s)\n", \
			__FILE__, __LINE__, #expr, rc_, mdb_strerror(rc_), ex_, mdb_strerror(ex_)); \
	} } while (0)

#define OK(expr) RC(expr, 0)

static const char *scratch;
static int ndirs;

/* A new, empty directory under the scratch directory. */
static const char *new_dir(void)
{
	static char path[8][1024];
	char *p = path[ndirs % 8];
	snprintf(p, sizeof(path[0]), "%s/env%d", scratch, ndirs++);
	if (mkdir(p, 0755) != 0) {
		fprintf(stderr, "mkdir %s: %s\n", p, strerror(errno));
		exit(2);
	}
	return p;
}

/* The format of the environment at dir as rlmdb_sniff_format sees it. */
static unsigned sniffed(const char *dir, unsigned flags)
{
	unsigned f = 99;
	OK(rlmdb_sniff_format(dir, flags, &f));
	return f;
}

/* Opens dir with an optional format request (0: none) and page size (0:
 * none); returns the mdb_env_open result, the handle in *envp (closed and
 * NULL on failure). */
static int open_env(const char *dir, unsigned format, unsigned psize, unsigned flags,
	MDB_env **envp)
{
	MDB_env *env;
	int rc;
	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 8));
	OK(mdb_env_set_mapsize(env, 256u << 20));
	if (format)
		OK(rlmdb_env_set_format(env, format));
	if (psize)
		OK(rlmdb_env_set_pagesize(env, psize));
	rc = mdb_env_open(env, dir, flags, 0644);
	if (rc) {
		mdb_env_close(env);
		env = NULL;
	}
	*envp = env;
	return rc;
}

/* The format of a new environment created in a new directory without a
 * request. */
static unsigned new_env_format(void)
{
	MDB_env *env;
	unsigned f = 99;
	OK(open_env(new_dir(), 0, 0, 0, &env));
	if (env) {
		OK(rlmdb_env_get_format(env, &f));
		mdb_env_close(env);
	}
	return f;
}

/* C 1.0's mdb_env_get_maxkeysize for pages of psize bytes:
 * me_nodemax - (NODESIZE + sizeof(MDB_db)). */
static int maxkey10(long psize)
{
	return (int)((((psize - 24) / 2) & ~1L) - 2 - 56);
}

static void test_constants(void)
{
	static const struct { int code; const char *text; } errs[] = {
		{ MDB_PROBLEM, "MDB_PROBLEM: Unexpected problem - txn should abort" },
		{ MDB_BAD_CHECKSUM, "MDB_BAD_CHECKSUM: Page checksum mismatch" },
		{ MDB_CRYPTO_FAIL, "MDB_CRYPTO_FAIL: Page encryption or decryption failed" },
		{ MDB_ENV_ENCRYPTION, "MDB_ENV_ENCRYPTION: Environment encryption mismatch" },
		{ MDB_TXN_PENDING, "MDB_TXN_PENDING: Transaction already prepared, must abort or commit" },
		{ MDB_CANT_ROLLBACK, "MDB_CANT_ROLLBACK: Environment can't rollback last transaction" },
		{ MDB_DBIS_BUSY, "MDB_DBIS_BUSY: Can't drop main DBI while other DBIs are open" },
		{ MDB_SHORT_WRITE, "MDB_SHORT_WRITE: Fewer bytes were written than requested" },
		{ MDB_ENV_BUSY, "MDB_ENV_BUSY: Environment is busy, can't use previous snapshot" },
		{ MDB_IS_READONLY, "MDB_IS_READONLY: Can't write in readonly txn or environment" },
		{ MDB_ADDR_BUSY, "MDB_ADDR_BUSY: Requested map address is unavailable" },
	};
	size_t i;

	CHECK(RLMDB_FORMAT_UNKNOWN == 0 && RLMDB_FORMAT_09 == 9 && RLMDB_FORMAT_10 == 10);
	/* The values of LMDB 1.0's lmdb.h. */
	CHECK(MDB_ENCRYPT == 0x2000 && MDB_PREVSNAPSHOT == 0x2000000);
	CHECK(MDB_PROBLEM == -30779 && MDB_ADDR_BUSY == -30769);
	CHECK(MDB_PROBLEM == MDB_LAST_ERRCODE + 1);
	for (i = 0; i < sizeof(errs) / sizeof(errs[0]); i++) {
		CHECK(errs[i].code == MDB_PROBLEM + (int)i);
		CHECK(!strcmp(mdb_strerror(errs[i].code), errs[i].text));
	}
	CHECK(!strcmp(mdb_strerror(MDB_ADDR_BUSY + 1), "Invalid error code"));
	CHECK(rlmdb_has_capability("default-format-env") == 1);
	CHECK(rlmdb_has_capability("format-1.0") == 1);
}

static void test_set_get(void)
{
	MDB_env *env;
	unsigned f = 99;
	long os = sysconf(_SC_PAGESIZE);
	const char *dir;

	if (os > 65536)
		os = 65536;
	RC(rlmdb_env_set_format(NULL, RLMDB_FORMAT_10), EINVAL);
	RC(rlmdb_env_get_format(NULL, &f), EINVAL);
	RC(rlmdb_env_set_pagesize(NULL, 4096), EINVAL);

	OK(mdb_env_create(&env));
	RC(rlmdb_env_get_format(env, NULL), EINVAL);
	OK(rlmdb_env_get_format(env, &f));
	CHECK(f == RLMDB_FORMAT_UNKNOWN);
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	OK(rlmdb_env_set_format(env, RLMDB_FORMAT_10));
	OK(rlmdb_env_get_format(env, &f));
	CHECK(f == RLMDB_FORMAT_10);
	/* Before the open: the 1.0 limit for the page size a new environment
	 * gets (C 1.0 says 0 here). */
	CHECK(mdb_env_get_maxkeysize(env) == maxkey10(os));
	OK(rlmdb_env_set_pagesize(env, 1024));
	CHECK(mdb_env_get_maxkeysize(env) == 442);
	OK(rlmdb_env_set_pagesize(env, 65536));
	CHECK(mdb_env_get_maxkeysize(env) == 32698);
	RC(rlmdb_env_set_format(env, 1), EINVAL);
	RC(rlmdb_env_set_format(env, 11), EINVAL);
	RC(rlmdb_env_set_format(env, 0x109), EINVAL);
	OK(rlmdb_env_get_format(env, &f));
	CHECK(f == RLMDB_FORMAT_10);
	OK(rlmdb_env_set_format(env, RLMDB_FORMAT_09));
	OK(rlmdb_env_get_format(env, &f));
	CHECK(f == RLMDB_FORMAT_09);
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	OK(rlmdb_env_set_format(env, RLMDB_FORMAT_UNKNOWN));
	OK(rlmdb_env_get_format(env, &f));
	CHECK(f == RLMDB_FORMAT_UNKNOWN);
	/* mdb_env_set_pagesize's checks (C 1.0). */
	RC(rlmdb_env_set_pagesize(env, 0), EINVAL);
	RC(rlmdb_env_set_pagesize(env, 128), EINVAL);
	RC(rlmdb_env_set_pagesize(env, 255), EINVAL);
	RC(rlmdb_env_set_pagesize(env, 257), EINVAL);
	RC(rlmdb_env_set_pagesize(env, 3072), EINVAL);
	RC(rlmdb_env_set_pagesize(env, 131072), EINVAL);
	OK(rlmdb_env_set_pagesize(env, 256));
	mdb_env_close(env);

	/* A 1.0 environment with 1 KiB pages. */
	dir = new_dir();
	OK(open_env(dir, RLMDB_FORMAT_10, 1024, 0, &env));
	if (env) {
		MDB_stat st;
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_10);
		CHECK(mdb_env_get_maxkeysize(env) == 442);
		OK(mdb_env_stat(env, &st));
		CHECK(st.ms_psize == 1024);
		RC(rlmdb_env_set_format(env, RLMDB_FORMAT_10), EINVAL);
		RC(rlmdb_env_set_pagesize(env, 1024), EINVAL);
		mdb_env_close(env);
	}
	CHECK(sniffed(dir, 0) == RLMDB_FORMAT_10);
	/* Opened without a request: the file decides, its page size too. */
	OK(open_env(dir, 0, 0, 0, &env));
	if (env) {
		MDB_stat st;
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_10);
		CHECK(mdb_env_get_maxkeysize(env) == 442);
		OK(mdb_env_stat(env, &st));
		CHECK(st.ms_psize == 1024);
		mdb_env_close(env);
	}
	/* A page size for an existing 1.0 file is ignored, as in C 1.0. */
	OK(open_env(dir, RLMDB_FORMAT_10, 4096, MDB_RDONLY, &env));
	if (env) {
		MDB_stat st;
		OK(mdb_env_stat(env, &st));
		CHECK(st.ms_psize == 1024);
		mdb_env_close(env);
	}
	/* The other format is refused, read-only too; the lock file is not
	 * touched first. */
	RC(open_env(dir, RLMDB_FORMAT_09, 0, 0, &env), MDB_VERSION_MISMATCH);
	RC(open_env(dir, RLMDB_FORMAT_09, 0, MDB_RDONLY, &env), MDB_VERSION_MISMATCH);

	/* A 0.9 environment, the default. */
	dir = new_dir();
	OK(open_env(dir, 0, 0, 0, &env));
	if (env) {
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_09);
		CHECK(mdb_env_get_maxkeysize(env) == 511);
		mdb_env_close(env);
	}
	CHECK(sniffed(dir, 0) == RLMDB_FORMAT_09);
	RC(open_env(dir, RLMDB_FORMAT_10, 0, 0, &env), MDB_VERSION_MISMATCH);
	/* 0.9 has no page size setting. */
	RC(open_env(dir, 0, 4096, 0, &env), EINVAL);
	RC(open_env(new_dir(), 0, 4096, 0, &env), EINVAL);
	RC(open_env(new_dir(), RLMDB_FORMAT_09, 4096, 0, &env), EINVAL);
}

static void test_sniff(void)
{
	char path[1100];
	unsigned f = 99;
	unsigned char head[64];
	const char *dir;
	MDB_env *env;
	FILE *fp;

	RC(rlmdb_sniff_format(NULL, 0, &f), EINVAL);
	RC(rlmdb_sniff_format(scratch, 0, NULL), EINVAL);
	/* No data file, or an empty one: a new environment. */
	CHECK(sniffed(new_dir(), 0) == RLMDB_FORMAT_UNKNOWN);
	snprintf(path, sizeof(path), "%s/nonexistent", scratch);
	CHECK(sniffed(path, 0) == RLMDB_FORMAT_UNKNOWN);
	CHECK(sniffed(path, MDB_NOSUBDIR) == RLMDB_FORMAT_UNKNOWN);
	dir = new_dir();
	snprintf(path, sizeof(path), "%s/data.mdb", dir);
	fp = fopen(path, "w");
	CHECK(fp != NULL);
	if (fp)
		fclose(fp);
	CHECK(sniffed(dir, 0) == RLMDB_FORMAT_UNKNOWN);

	/* MDB_NOSUBDIR: path is the data file. */
	dir = new_dir();
	snprintf(path, sizeof(path), "%s/env10", dir);
	OK(open_env(path, RLMDB_FORMAT_10, 0, MDB_NOSUBDIR, &env));
	if (env)
		mdb_env_close(env);
	CHECK(sniffed(path, MDB_NOSUBDIR) == RLMDB_FORMAT_10);
	/* Other flags are ignored. */
	CHECK(sniffed(path, MDB_NOSUBDIR | MDB_RDONLY | MDB_NOLOCK) == RLMDB_FORMAT_10);
	/* Without MDB_NOSUBDIR the file is taken as a directory. */
	f = 99;
	CHECK(rlmdb_sniff_format(path, 0, &f) != 0 && f == 99);

	/* Not an LMDB file, and LMDB data version 2 (lmdb-js prereleases). */
	memset(head, 0x5a, sizeof(head));
	snprintf(path, sizeof(path), "%s/junk", dir);
	fp = fopen(path, "w");
	if (fp) {
		fwrite(head, 1, sizeof(head), fp);
		fclose(fp);
	}
	f = 99;
	RC(rlmdb_sniff_format(path, MDB_NOSUBDIR, &f), MDB_INVALID);
	CHECK(f == 99);
	{
		unsigned magic = 0xBEEFC0DE, version = 2;
		memset(head, 0, sizeof(head));
		memcpy(head + 16, &magic, 4);
		memcpy(head + 20, &version, 4);
	}
	snprintf(path, sizeof(path), "%s/v2", dir);
	fp = fopen(path, "w");
	if (fp) {
		fwrite(head, 1, sizeof(head), fp);
		fclose(fp);
	}
	RC(rlmdb_sniff_format(path, MDB_NOSUBDIR, &f), MDB_VERSION_MISMATCH);
	CHECK(f == 99);
}

static void test_defaults(void)
{
	MDB_env *env;
	const char *old09, *old10;
	unsigned f;

	unsetenv("RLMDB_DEFAULT_FORMAT");
	OK(rlmdb_set_default_format(RLMDB_FORMAT_UNKNOWN));
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	RC(rlmdb_set_default_format(1), EINVAL);
	RC(rlmdb_set_default_format(11), EINVAL);
	OK(rlmdb_set_default_format(RLMDB_FORMAT_10));
	CHECK(new_env_format() == RLMDB_FORMAT_10);
	OK(rlmdb_set_default_format(RLMDB_FORMAT_09));
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	OK(rlmdb_set_default_format(RLMDB_FORMAT_UNKNOWN));
	CHECK(new_env_format() == RLMDB_FORMAT_09);

	/* The experimental variable, read at every open. */
	setenv("RLMDB_DEFAULT_FORMAT", "1.0", 1);
	CHECK(new_env_format() == RLMDB_FORMAT_10);
	setenv("RLMDB_DEFAULT_FORMAT", "0.9", 1);
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	setenv("RLMDB_DEFAULT_FORMAT", "1.0x", 1);
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	setenv("RLMDB_DEFAULT_FORMAT", "", 1);
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	setenv("RLMDB_DEFAULT_FORMAT", "1.0", 1);
	/* The process default takes precedence, a request more so. */
	OK(rlmdb_set_default_format(RLMDB_FORMAT_09));
	CHECK(new_env_format() == RLMDB_FORMAT_09);
	OK(rlmdb_set_default_format(RLMDB_FORMAT_UNKNOWN));
	CHECK(new_env_format() == RLMDB_FORMAT_10);
	OK(open_env(new_dir(), RLMDB_FORMAT_09, 0, 0, &env));
	if (env) {
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_09);
		mdb_env_close(env);
	}
	/* A page size with a default of 1.0 makes a 1.0 environment. */
	OK(open_env(new_dir(), 0, 512, 0, &env));
	if (env) {
		MDB_stat st;
		OK(mdb_env_stat(env, &st));
		CHECK(st.ms_psize == 512);
		mdb_env_close(env);
	}

	/* Existing files keep their format whatever the defaults say. */
	unsetenv("RLMDB_DEFAULT_FORMAT");
	old09 = new_dir();
	OK(open_env(old09, 0, 0, 0, &env));
	if (env)
		mdb_env_close(env);
	old10 = new_dir();
	OK(open_env(old10, RLMDB_FORMAT_10, 0, 0, &env));
	if (env)
		mdb_env_close(env);
	setenv("RLMDB_DEFAULT_FORMAT", "1.0", 1);
	OK(open_env(old09, 0, 0, 0, &env));
	if (env) {
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_09);
		mdb_env_close(env);
	}
	OK(rlmdb_set_default_format(RLMDB_FORMAT_09));
	OK(open_env(old10, 0, 0, MDB_RDONLY, &env));
	if (env) {
		OK(rlmdb_env_get_format(env, &f));
		CHECK(f == RLMDB_FORMAT_10);
		mdb_env_close(env);
	}
	OK(rlmdb_set_default_format(RLMDB_FORMAT_UNKNOWN));
	unsetenv("RLMDB_DEFAULT_FORMAT");
	CHECK(new_env_format() == RLMDB_FORMAT_09);
}

static void test_refused_flags(void)
{
	MDB_env *env;
	const char *dir;

	/* MDB_PREVSNAPSHOT: unknown to 0.9, not implemented for 1.0. */
	RC(open_env(new_dir(), 0, 0, MDB_PREVSNAPSHOT, &env), EINVAL);
	RC(open_env(new_dir(), RLMDB_FORMAT_10, 0, MDB_PREVSNAPSHOT, &env), EINVAL);
	dir = new_dir();
	OK(open_env(dir, RLMDB_FORMAT_10, 0, 0, &env));
	if (env) {
		RC(mdb_env_set_flags(env, MDB_ENCRYPT, 1), EINVAL);
		RC(mdb_env_set_flags(env, MDB_ENCRYPT, 0), EINVAL);
		RC(mdb_env_set_flags(env, MDB_PREVSNAPSHOT, 1), EINVAL);
		mdb_env_close(env);
	}
	RC(open_env(dir, 0, 0, MDB_PREVSNAPSHOT, &env), EINVAL);
	RC(open_env(dir, 0, 0, MDB_PREVSNAPSHOT | MDB_RDONLY, &env), EINVAL);
	RC(open_env(dir, 0, 0, MDB_ENCRYPT, &env), EINVAL);
	OK(mdb_env_create(&env));
	RC(mdb_env_set_flags(env, MDB_ENCRYPT, 1), EINVAL);
	mdb_env_close(env);
}

/* ---- keys and data beyond LMDB 0.9's limits ------------------------------ */

/* A deterministic buffer of n bytes for item `seed`. */
static void fill(unsigned char *buf, size_t n, unsigned seed)
{
	size_t i;
	unsigned x = seed * 2654435761u + 1;
	for (i = 0; i < n; i++) {
		x = x * 1103515245u + 12345u;
		buf[i] = (unsigned char)(x >> 16);
	}
}

static MDB_val val(void *p, size_t n)
{
	MDB_val v;
	v.mv_data = p;
	v.mv_size = n;
	return v;
}

static int eq(MDB_val v, const unsigned char *p, size_t n)
{
	return v.mv_size == n && !memcmp(v.mv_data, p, n);
}

/* Keys of maxkey bytes, data of 3/8 of a page (inline: up to 24 KiB, above
 * the 16 KiB 0.9 limit on 64 KiB pages) and of two pages (overflow), through
 * mdb_put, mdb_cursor_put (MDB_CURRENT, MDB_RESERVE, MDB_MULTIPLE),
 * mdb_get, mdb_del and mdb_dbi_open, with inputs that point into the
 * transaction's own dirty pages. Checked again after a reopen. */
static void test_large_items(unsigned psize)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_cursor *cur;
	MDB_dbi plain, dups, fixed, beta;
	MDB_val k, d, kk, dd, multi[2];
	unsigned char *key, *key2, *big, *inl, *items;
	size_t maxkey, inline_len = psize / 8 * 3, big_len = 2 * (size_t)psize;
	size_t item = 0, nitems = 0, i;
	unsigned f;
	const char *dir = new_dir();
	int pass;

	OK(open_env(dir, RLMDB_FORMAT_10, psize, 0, &env));
	if (!env)
		return;
	maxkey = (size_t)mdb_env_get_maxkeysize(env);
	CHECK((int)maxkey == maxkey10(psize));
	key = malloc(maxkey + 1);
	key2 = malloc(maxkey);
	big = malloc(big_len);
	inl = malloc(inline_len);
	/* MDB_MULTIPLE: as many DUPFIXED items as fill most of a page. */
	item = maxkey < 1000 ? maxkey : 1000;
	nitems = (psize - 64) / item;
	items = malloc(item * nitems);
	fill(key, maxkey + 1, 1);
	fill(key2, maxkey, 2);
	key[0] = 1; /* key sorts before key2 */
	key2[0] = 2;
	fill(big, big_len, 3);
	fill(inl, inline_len, 4);
	for (i = 0; i < nitems; i++) {
		fill(items + i * item, item, 100 + (unsigned)i);
		items[i * item] = (unsigned char)i; /* sorted, distinct */
	}

	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, "plain", MDB_CREATE, &plain));
	OK(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dups));
	OK(mdb_dbi_open(txn, "fixed", MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED, &fixed));

	/* The longest key; one byte more is MDB_BAD_VALSIZE. */
	k = val(key, maxkey + 1);
	d = val(inl, 10);
	RC(mdb_put(txn, plain, &k, &d, 0), MDB_BAD_VALSIZE);
	k = val(key, maxkey);
	d = val(inl, inline_len);
	OK(mdb_put(txn, plain, &k, &d, 0));
	k = val(key2, maxkey);
	d = val(big, big_len);
	OK(mdb_put(txn, plain, &k, &d, 0));
	/* DUPSORT items are limited like keys: longer than 511 in 1.0. */
	k = val(key2, 8);
	d = val(key, maxkey);
	OK(mdb_put(txn, dups, &k, &d, 0));
	d = val(key, maxkey + 1);
	RC(mdb_put(txn, dups, &k, &d, 0), MDB_BAD_VALSIZE);
	d = val(key2, maxkey);
	OK(mdb_put(txn, dups, &k, &d, 0));

	/* Inputs that point into the dirty leaf: the key and data of the
	 * first record, put again under the second key's prefix, and the
	 * first record replaced through the cursor (MDB_CURRENT) by data of
	 * another size with its own key as the key. */
	OK(mdb_cursor_open(txn, plain, &cur));
	OK(mdb_cursor_get(cur, &kk, &dd, MDB_FIRST));
	CHECK(eq(kk, key, maxkey) && eq(dd, inl, inline_len));
	/* Same key, shorter inline data taken from itself. */
	dd.mv_size = inline_len / 2;
	OK(mdb_cursor_put(cur, &kk, &dd, MDB_CURRENT));
	OK(mdb_cursor_get(cur, &kk, &dd, MDB_GET_CURRENT));
	CHECK(eq(kk, key, maxkey) && eq(dd, inl, inline_len / 2));
	/* A new key made of the dirty page's bytes (the first maxkey - 1 of
	 * the cursor's key), data from the same page. */
	OK(mdb_cursor_get(cur, &kk, &dd, MDB_FIRST));
	kk.mv_size = maxkey - 1;
	if (dd.mv_size > 100)
		dd.mv_size = 100;
	OK(mdb_put(txn, plain, &kk, &dd, 0));
	mdb_cursor_close(cur);

	/* MDB_RESERVE of inline data larger than 16 KiB on large pages. */
	k = val(key2, 16);
	d = val(NULL, inline_len);
	OK(mdb_put(txn, plain, &k, &d, MDB_RESERVE));
	CHECK(d.mv_data != NULL && d.mv_size == inline_len);
	if (d.mv_data)
		memcpy(d.mv_data, inl, inline_len);

	/* MDB_MULTIPLE: a page of items, then the same items again under
	 * another key straight from MDB_GET_MULTIPLE (the dirty page). */
	OK(mdb_cursor_open(txn, fixed, &cur));
	k = val(key2, 4);
	multi[0] = val(items, item);
	multi[1] = val(NULL, nitems);
	OK(mdb_cursor_put(cur, &k, multi, MDB_MULTIPLE));
	CHECK(multi[1].mv_size == nitems);
	OK(mdb_cursor_get(cur, &k, &dd, MDB_SET));
	OK(mdb_cursor_get(cur, &k, &dd, MDB_GET_MULTIPLE));
	CHECK(dd.mv_size % item == 0 && dd.mv_size / item <= nitems);
	{
		size_t got = dd.mv_size / item;
		k = val(key2, 5);
		multi[0] = val(dd.mv_data, item);
		multi[1] = val(NULL, got);
		OK(mdb_cursor_put(cur, &k, multi, MDB_MULTIPLE));
		CHECK(multi[1].mv_size == got);
	}
	mdb_cursor_close(cur);

	/* mdb_dbi_open with a name that points into a dirty page: the value
	 * of a main-DB record, NUL included. */
	{
		MDB_dbi main;
		char name[] = "beta";
		OK(mdb_dbi_open(txn, NULL, 0, &main));
		k = val("zz-name", 7);
		d = val(name, sizeof(name));
		OK(mdb_put(txn, main, &k, &d, 0));
		OK(mdb_get(txn, main, &k, &d));
		CHECK(d.mv_size == sizeof(name) && !strcmp(d.mv_data, "beta"));
		OK(mdb_dbi_open(txn, d.mv_data, MDB_CREATE, &beta));
		k = val(key, maxkey);
		d = val(inl, 3);
		OK(mdb_put(txn, beta, &k, &d, 0));
	}
	OK(mdb_txn_commit(txn));

	for (pass = 0; pass < 2; pass++) {
		if (pass == 1) {
			/* Reopened without a request: a 1.0 file, sniffed. */
			mdb_env_close(env);
			OK(open_env(dir, 0, 0, MDB_RDONLY, &env));
			if (!env)
				break;
			OK(rlmdb_env_get_format(env, &f));
			CHECK(f == RLMDB_FORMAT_10);
			CHECK((size_t)mdb_env_get_maxkeysize(env) == maxkey);
		}
		OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		OK(mdb_dbi_open(txn, "plain", 0, &plain));
		OK(mdb_dbi_open(txn, "dups", 0, &dups));
		OK(mdb_dbi_open(txn, "fixed", 0, &fixed));
		OK(mdb_dbi_open(txn, "beta", 0, &beta));
		k = val(key, maxkey);
		OK(mdb_get(txn, plain, &k, &d));
		CHECK(eq(d, inl, inline_len / 2));
		/* The key made of the page's bytes, with that data's start. */
		k = val(key, maxkey - 1);
		OK(mdb_get(txn, plain, &k, &d));
		CHECK(eq(d, inl, 100 < inline_len / 2 ? 100 : inline_len / 2));
		k = val(key2, maxkey);
		OK(mdb_get(txn, plain, &k, &d));
		CHECK(eq(d, big, big_len));
		k = val(key2, 16);
		OK(mdb_get(txn, plain, &k, &d));
		CHECK(eq(d, inl, inline_len));
		k = val(key, maxkey);
		OK(mdb_get(txn, beta, &k, &d));
		CHECK(eq(d, inl, 3));
		OK(mdb_cursor_open(txn, dups, &cur));
		k = val(key2, 8);
		{
			size_t n = 0;
			OK(mdb_cursor_get(cur, &k, &d, MDB_SET));
			OK(mdb_cursor_count(cur, &n));
			CHECK(n == 2);
			d = val(key, maxkey);
			OK(mdb_cursor_get(cur, &k, &d, MDB_GET_BOTH));
			d = val(key2, maxkey);
			OK(mdb_cursor_get(cur, &k, &d, MDB_GET_BOTH));
		}
		mdb_cursor_close(cur);
		OK(mdb_cursor_open(txn, fixed, &cur));
		{
			size_t n = 0;
			k = val(key2, 4);
			OK(mdb_cursor_get(cur, &k, &d, MDB_SET));
			OK(mdb_cursor_count(cur, &n));
			CHECK(n == nitems);
			k = val(key2, 5);
			OK(mdb_cursor_get(cur, &k, &d, MDB_SET));
			OK(mdb_cursor_get(cur, &k, &d, MDB_FIRST_DUP));
			CHECK(eq(d, items, item));
		}
		mdb_cursor_close(cur);
		mdb_txn_abort(txn);
	}

	/* Deletes with long keys and long DUPSORT data. */
	if (env) {
		mdb_env_close(env);
		OK(open_env(dir, RLMDB_FORMAT_10, 0, 0, &env));
	}
	if (env) {
		OK(mdb_txn_begin(env, NULL, 0, &txn));
		OK(mdb_dbi_open(txn, "plain", 0, &plain));
		OK(mdb_dbi_open(txn, "dups", 0, &dups));
		k = val(key2, maxkey);
		OK(mdb_del(txn, plain, &k, NULL));
		RC(mdb_get(txn, plain, &k, &d), MDB_NOTFOUND);
		k = val(key2, 8);
		d = val(key, maxkey);
		OK(mdb_del(txn, dups, &k, &d));
		RC(mdb_del(txn, dups, &k, &d), MDB_NOTFOUND);
		OK(mdb_get(txn, dups, &k, &d));
		CHECK(eq(d, key2, maxkey));
		OK(mdb_txn_commit(txn));
		mdb_env_close(env);
	}
	free(key);
	free(key2);
	free(big);
	free(inl);
	free(items);
}

int main(int argc, char **argv)
{
	unsigned sizes[] = { 1024, 4096, 65536 };
	size_t i;

	if (argc != 2) {
		fprintf(stderr, "usage: capi_format <scratch-dir>\n");
		return 2;
	}
	scratch = argv[1];
	unsetenv("RLMDB_DEFAULT_FORMAT");
	test_constants();
	test_set_get();
	test_sniff();
	test_defaults();
	test_refused_flags();
	for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++)
		test_large_items(sizes[i]);
	printf("capi_format: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
