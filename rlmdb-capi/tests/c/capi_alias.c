/* A second mdb_env_open of an environment that the process has open, under
 * other names, through the C API against the Rust library.
 *
 *   capi_alias <scratch-dir>
 *
 * The harness (tests/capi_alias.rs) leaves empty files a/data.mdb and
 * a/lock.mdb in the scratch directory, with hard links of both in b and of
 * the lock file alone in c (a bind mount would show them the same way; the
 * test shims have no link()). The program opens a, which sets the files
 * up, commits a value on overflow pages and keeps a read transaction that
 * holds it. Opening b (read-write, read-only, MDB_NOLOCK) and c (read-write
 * without a data file, then read-write and read-only with a copy of a's)
 * fails with EBUSY, and the first handle keeps its reader slot
 * (mdb_reader_list, me_numreaders), the value its reader holds and the
 * txnid of its next writer, which keeps the reader's pages. C allows the
 * second open, which lmdb.h forbids: it would set the lock file up again
 * (record locks do not conflict within a process) and re-initialise the
 * reader table under the reader. So there is no C build.
 *
 * Prints "capi_alias: N checks, F failures" and exits 1 on failures.
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

/* Size of the value the reader holds: on overflow pages. */
#define VALSIZE 32000

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

/* The first handle as it was: me_numreaders `nreaders`, one reader in
 * mdb_reader_list (this process, at txnid 1) and the held value as copied. */
static void check_first(MDB_env *env, unsigned nreaders, const MDB_val *held,
	const unsigned char *copy, const char *after)
{
	MDB_envinfo info;
	struct buf list = { "", 0 };
	char pid[32];
	const char *line;
	int before = failures;

	OK(mdb_env_info(env, &info));
	CHECK(info.me_numreaders == nreaders);
	OK(mdb_reader_list(env, collect, &list));
	snprintf(pid, sizeof(pid), "\n%10d ", (int)getpid());
	line = strstr(list.s, pid);
	CHECK(line != NULL && strchr(line + 1, '\n') == list.s + list.n - 1);
	CHECK(list.n >= 3 && strcmp(list.s + list.n - 3, " 1\n") == 0);
	CHECK(held->mv_size == VALSIZE && memcmp(held->mv_data, copy, VALSIZE) == 0);
	if (failures != before)
		fprintf(stderr, "  after %s; reader list:\n%s", after, list.s);
}

/* mdb_env_open of `path` with `flags` must fail with EBUSY. */
static void refused(const char *path, unsigned flags)
{
	MDB_env *env;
	OK(mdb_env_create(&env));
	RC(mdb_env_open(env, path, flags, 0664), EBUSY);
	mdb_env_close(env);
}

int main(int argc, char **argv)
{
	static const unsigned b_flags[] = {
		0, MDB_RDONLY, MDB_NOLOCK, MDB_NOLOCK | MDB_RDONLY
	};
	char a[1024], b[1024], c[1024], c_data[1024];
	MDB_env *env;
	MDB_txn *txn, *r;
	MDB_dbi dbi;
	MDB_envinfo info;
	MDB_val key, data, held;
	unsigned char *value, *copy;
	unsigned i, nreaders;
	struct stat sb;

	if (argc != 2) {
		fprintf(stderr, "usage: capi_alias <scratch-dir>\n");
		return 2;
	}
	setvbuf(stdout, NULL, _IOLBF, 0);
	snprintf(a, sizeof(a), "%s/a", argv[1]);
	snprintf(b, sizeof(b), "%s/b", argv[1]);
	snprintf(c, sizeof(c), "%s/c", argv[1]);
	snprintf(c_data, sizeof(c_data), "%s/c/data.mdb", argv[1]);
	value = malloc(VALSIZE);
	copy = malloc(VALSIZE);
	if (!value || !copy)
		return 2;

	OK(mdb_env_create(&env));
	OK(mdb_env_open(env, a, MDB_NOSYNC, 0664));
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	OK(mdb_dbi_open(txn, NULL, 0, &dbi));
	key.mv_size = 3;
	key.mv_data = "key";
	data.mv_size = VALSIZE;
	data.mv_data = value;
	memset(value, 11, VALSIZE);
	OK(mdb_put(txn, dbi, &key, &data, 0));
	OK(mdb_txn_commit(txn));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	OK(mdb_get(r, dbi, &key, &held));
	CHECK(held.mv_size == VALSIZE);
	if (held.mv_size != VALSIZE)
		return 1;
	memcpy(copy, held.mv_data, VALSIZE);
	OK(mdb_env_info(env, &info));
	nreaders = info.me_numreaders;
	CHECK(nreaders == 1);
	check_first(env, nreaders, &held, copy, "the read transaction began");

	/* Both files under another name: refused at the data file. */
	for (i = 0; i < sizeof(b_flags) / sizeof(b_flags[0]); i++) {
		refused(b, b_flags[i]);
		check_first(env, nreaders, &held, copy, "opening b");
	}

	/* The lock file alone. Without a data file the lock file comes first,
	 * so the data file is not created; then with a copy of a's. */
	refused(c, 0);
	check_first(env, nreaders, &held, copy, "opening c without a data file");
	CHECK(stat(c_data, &sb) == -1 && errno == ENOENT);
	OK(mdb_env_copy(env, c));
	OK(mdb_env_info(env, &info));
	nreaders = info.me_numreaders;
	refused(c, 0);
	check_first(env, nreaders, &held, copy, "opening c read-write");
	refused(c, MDB_RDONLY);
	check_first(env, nreaders, &held, copy, "opening c read-only");

	/* The handle carries on: its next writer follows the last commit and
	 * keeps the reader's pages. */
	OK(mdb_txn_begin(env, NULL, 0, &txn));
	CHECK(mdb_txn_id(txn) == 2);
	memset(value, 22, VALSIZE);
	OK(mdb_put(txn, dbi, &key, &data, 0));
	OK(mdb_txn_commit(txn));
	CHECK(memcmp(held.mv_data, copy, VALSIZE) == 0);
	mdb_txn_abort(r);
	mdb_env_close(env);

	/* Without the first handle, b opens the environment. */
	OK(mdb_env_create(&env));
	OK(mdb_env_open(env, b, 0, 0664));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &r));
	CHECK(mdb_txn_id(r) == 2);
	OK(mdb_get(r, dbi, &key, &data));
	CHECK(data.mv_size == VALSIZE && memcmp(data.mv_data, value, VALSIZE) == 0);
	mdb_txn_abort(r);
	mdb_env_close(env);

	free(value);
	free(copy);
	printf("capi_alias: %d checks, %d failures\n", checks, failures);
	return failures != 0;
}
