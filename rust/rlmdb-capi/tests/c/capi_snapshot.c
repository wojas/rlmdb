/* A reader that holds snapshots while C writers (mtest*) change the
 * environment; driven over stdin/stdout by tests/concurrency.rs.
 *
 *   capi_snapshot stability <env-dir> <dump-file>
 *   capi_snapshot resize <env-dir>
 *
 * Both modes create the environment (so it has no recorded map address and
 * C writers may reopen it with MDB_FIXEDMAP) with MDB_NOTLS (one thread
 * holds several read transactions), print "READY ...", and then wait for
 * "step" lines; each "step" means the driver has run a writer.
 *
 * stability: holds T0 on the empty environment; after the first step
 *   checks T0 is unchanged, begins T1, records every main-DB pair (the
 *   pointers into the map and copies of the bytes) and writes them as hex
 *   to <dump-file>; after the second step checks that T0 and T1 still see
 *   exactly the same bytes at the same addresses, then resets and renews
 *   T1 onto the latest snapshot.
 * resize: maps only the two meta pages; after the step (a writer grew the
 *   file) a new transaction and a renew must fail with MDB_MAP_RESIZED, and
 *   mdb_env_set_mapsize(env, 0) must recover.
 *
 * Prints "DONE <checks> <failures>" at the end; exit status 1 on failures.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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

#ifdef LMDB_RUST
#define GAP(name, cond) do { if (cond) { failures++; \
	fprintf(stderr, "%s:%d: known gap \"%s\" now behaves like C: " \
		"update the test and rust/rlmdb-capi/README.md\n", __FILE__, __LINE__, name); \
	} else printf("known gap: %s\n", name); } while (0)
#else
#define GAP(name, cond) CHECK(cond)
#endif

static void wait_step(void)
{
	char line[64];
	if (!fgets(line, sizeof(line), stdin) || strncmp(line, "step", 4)) {
		fprintf(stderr, "expected \"step\"\n");
		exit(2);
	}
}

static size_t entries(MDB_txn *txn)
{
	MDB_stat st;
	st.ms_entries = (size_t)-1;
	OK(mdb_stat(txn, 1, &st));
	return st.ms_entries;
}

struct pair { MDB_val k, d; char *kc, *dc; };

static void hex(FILE *f, MDB_val v)
{
	size_t i;
	for (i = 0; i < v.mv_size; i++)
		fprintf(f, "%02x", ((unsigned char *)v.mv_data)[i]);
}

static int stability(const char *path, const char *dumpfile)
{
	MDB_env *env;
	MDB_txn *t0, *t1;
	MDB_cursor *c0, *c1;
	MDB_dbi dbi;
	MDB_val k, d;
	struct pair *pairs = NULL;
	size_t n = 0, cap = 0, i, id1;
	FILE *f;
	int rc;

	OK(mdb_env_create(&env));
	OK(mdb_env_set_maxdbs(env, 4));
	OK(mdb_env_set_mapsize(env, 64UL << 20));
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &t0));
	OK(mdb_dbi_open(t0, NULL, 0, &dbi));
	OK(mdb_cursor_open(t0, dbi, &c0));
	printf("READY %zu %zu\n", mdb_txn_id(t0), entries(t0));
	fflush(stdout);

	wait_step();
	/* T0 still reads the empty snapshot. */
	CHECK(mdb_txn_id(t0) == 0);
	CHECK(entries(t0) == 0);
	RC(mdb_cursor_get(c0, &k, &d, MDB_FIRST), MDB_NOTFOUND);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &t1));
	CHECK(mdb_txn_id(t1) > 0);
	OK(mdb_cursor_open(t1, dbi, &c1));
	f = fopen(dumpfile, "w");
	CHECK(f != NULL);
	while ((rc = mdb_cursor_get(c1, &k, &d, MDB_NEXT)) == 0) {
		if (n == cap) {
			cap = cap ? 2 * cap : 256;
			pairs = realloc(pairs, cap * sizeof(*pairs));
		}
		pairs[n].k = k;
		pairs[n].d = d;
		pairs[n].kc = malloc(k.mv_size + 1);
		pairs[n].dc = malloc(d.mv_size + 1);
		memcpy(pairs[n].kc, k.mv_data, k.mv_size);
		memcpy(pairs[n].dc, d.mv_data, d.mv_size);
		n++;
		if (f) {
			hex(f, k);
			fputc(' ', f);
			hex(f, d);
			fputc('\n', f);
		}
	}
	RC(rc, MDB_NOTFOUND);
	if (f)
		fclose(f);
	CHECK(n == entries(t1) && n > 0);
	id1 = mdb_txn_id(t1);
	printf("T1 %zu %zu\n", id1, n);
	fflush(stdout);

	wait_step();
	/* The writers committed more transactions; both snapshots are intact,
	 * byte for byte and at the same addresses (zero copy, pinned pages). */
	CHECK(mdb_txn_id(t0) == 0 && entries(t0) == 0);
	RC(mdb_cursor_get(c0, &k, &d, MDB_LAST), MDB_NOTFOUND);
	CHECK(mdb_txn_id(t1) == id1 && entries(t1) == n);
	for (i = 0; i < n; i++) {
		CHECK(!memcmp(pairs[i].k.mv_data, pairs[i].kc, pairs[i].k.mv_size));
		CHECK(!memcmp(pairs[i].d.mv_data, pairs[i].dc, pairs[i].d.mv_size));
	}
	i = 0;
	for (rc = mdb_cursor_get(c1, &k, &d, MDB_FIRST); rc == 0;
		 rc = mdb_cursor_get(c1, &k, &d, MDB_NEXT), i++) {
		CHECK(i < n && k.mv_data == pairs[i].k.mv_data && d.mv_data == pairs[i].d.mv_data);
	}
	CHECK(i == n);
	/* A reset + renew moves T1 to the latest snapshot. */
	mdb_txn_reset(t1);
	OK(mdb_txn_renew(t1));
	CHECK(mdb_txn_id(t1) > id1);
	OK(mdb_cursor_renew(t1, c1));
	printf("T1R %zu %zu\n", mdb_txn_id(t1), entries(t1));
	fflush(stdout);

	mdb_cursor_close(c0);
	mdb_txn_abort(t0);
	mdb_txn_abort(t1);
	mdb_cursor_close(c1);
	mdb_env_close(env);
	for (i = 0; i < n; i++) {
		free(pairs[i].kc);
		free(pairs[i].dc);
	}
	free(pairs);
	return 0;
}

static int resize(const char *path)
{
	MDB_env *env;
	MDB_txn *t0, *t1;
	MDB_envinfo info;
	MDB_stat st;

	OK(mdb_env_create(&env));
	OK(mdb_env_set_mapsize(env, 1));	/* raised to the two meta pages */
	OK(mdb_env_open(env, path, MDB_NOTLS, 0664));
	OK(mdb_env_stat(env, &st));
	OK(mdb_env_info(env, &info));
	CHECK(info.me_mapsize == 2 * (size_t)st.ms_psize);
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &t0));
	printf("READY %zu %zu\n", mdb_txn_id(t0), info.me_mapsize);
	fflush(stdout);

	wait_step();
	RC(mdb_txn_begin(env, NULL, MDB_RDONLY, &t1), MDB_MAP_RESIZED);
	mdb_txn_reset(t0);
	RC(mdb_txn_renew(t0), MDB_MAP_RESIZED);
	mdb_txn_abort(t0);
	OK(mdb_env_set_mapsize(env, 0));
	OK(mdb_env_info(env, &info));
	CHECK(info.me_mapsize == 10485760);	/* recorded by mtest */
	OK(mdb_txn_begin(env, NULL, MDB_RDONLY, &t1));
	CHECK(mdb_txn_id(t1) > 0 && entries(t1) > 0);
	mdb_txn_abort(t1);
	mdb_env_close(env);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc == 4 && !strcmp(argv[1], "stability"))
		stability(argv[2], argv[3]);
	else if (argc == 3 && !strcmp(argv[1], "resize"))
		resize(argv[2]);
	else {
		fprintf(stderr, "usage: capi_snapshot stability <env> <dump> | resize <env>\n");
		return 2;
	}
	printf("DONE %d %d\n", checks, failures);
	return failures ? 1 : 0;
}
