/* cbench: the C side of the LMDB benchmarks.
 *
 * Every workload here has a twin in ../src/workloads.rs that does the same
 * operations in the same order on the Rust library; the data generator is
 * the one of ../src/data.rs (same seeds, same bytes). See ../README.md.
 *
 * Usage: cbench <group> <dir> <n> <reps> <sync_commits>
 *
 * Runs one warm-up iteration and <reps> timed ones of each workload of
 * <group> in the environment directory <dir> (created; its data.mdb and
 * lock.mdb are removed before each fresh run and at the end) and prints one
 * line per workload:
 *
 *   RESULT <name> <ns>/<ops> <ns>/<ops> ...
 *
 * one <ns>/<ops> pair (elapsed nanoseconds, operations) per timed rep.
 */
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "lmdb.h"

#define CHK(expr)                                                              \
	do {                                                                   \
		int rc_ = (expr);                                              \
		if (rc_ != MDB_SUCCESS) {                                      \
			fprintf(stderr, "cbench: %s:%d: %s: %s\n", __FILE__,   \
			    __LINE__, #expr, mdb_strerror(rc_));               \
			exit(1);                                               \
		}                                                              \
	} while (0)

#define MAXREP 64
#define MAPSIZE ((size_t)4 << 30)
#define KEYLEN 16
#define VALLEN 100
#define BATCH 10000
#define OVF_BATCH 1000
#define DUPS 10
#define READERS 8
#define MIXED_READERS 4
#define MIXED_BATCH 100

/* ---- data generator (identical to src/data.rs) ------------------------ */

#define SEED_KEYS 0x5eed000000000001ULL
#define SEED_POOL 0x5eed000000000002ULL
#define SEED_DUPS 0x5eed000000000003ULL
#define S_GET 0x1000000000000000ULL
#define S_OVERWRITE 0x2000000000000000ULL
#define S_DELETE 0x3000000000000000ULL
#define S_DUP_GET 0x4000000000000000ULL
#define S_DUP_SCAN 0x5000000000000000ULL
#define S_OVF_GET 0x6000000000000000ULL
#define S_READER 0x7000000000000000ULL
#define S_MIXED 0x8000000000000000ULL
#define POOL_SIZE (3 * 65536)

static uint8_t pool[POOL_SIZE];

static uint64_t splitmix64(uint64_t x)
{
	uint64_t z = x + 0x9e3779b97f4a7c15ULL;
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	return z ^ (z >> 31);
}

static void put_be64(uint8_t *p, uint64_t v)
{
	int i;
	for (i = 0; i < 8; i++)
		p[i] = (uint8_t)(v >> (56 - 8 * i));
}

static void init_pool(void)
{
	size_t w;
	int b;
	for (w = 0; w < POOL_SIZE / 8; w++) {
		uint64_t v = splitmix64(SEED_POOL + w);
		for (b = 0; b < 8; b++)
			pool[w * 8 + b] = (uint8_t)(v >> (8 * b));
	}
}

/* Offset of the value bytes of item i in the pool. */
static size_t val_off(uint64_t i)
{
	return (size_t)((i * 64) & 0xffff);
}

static uint8_t *rand_keys(size_t n)
{
	uint8_t *k = malloc(n * KEYLEN);
	size_t i;
	for (i = 0; i < n; i++) {
		put_be64(k + i * KEYLEN, splitmix64(SEED_KEYS + 2 * i));
		put_be64(k + i * KEYLEN + 8, splitmix64(SEED_KEYS + 2 * i + 1));
	}
	return k;
}

static uint8_t *seq_keys(size_t n)
{
	uint8_t *k = malloc(n * KEYLEN);
	size_t i;
	for (i = 0; i < n; i++) {
		put_be64(k + i * KEYLEN, i);
		put_be64(k + i * KEYLEN + 8, splitmix64(i));
	}
	return k;
}

/* count indices in [0, n) from stream salt. */
static uint32_t *rand_idx(uint64_t salt, size_t count, size_t n)
{
	uint32_t *v = malloc(count * sizeof(*v));
	size_t j;
	for (j = 0; j < count; j++)
		v[j] = (uint32_t)(splitmix64(salt + j) % n);
	return v;
}

/* A permutation of [0, n) (Fisher-Yates from stream salt). */
static uint32_t *perm(uint64_t salt, size_t n)
{
	uint32_t *p = malloc(n * sizeof(*p));
	size_t j;
	for (j = 0; j < n; j++)
		p[j] = (uint32_t)j;
	for (j = n - 1; j > 0; j--) {
		size_t k = (size_t)(splitmix64(salt + j) % (j + 1));
		uint32_t t = p[j];
		p[j] = p[k];
		p[k] = t;
	}
	return p;
}

/* Duplicate j of DUPSORT key i: 100 bytes. */
static void dup_data(uint64_t i, uint64_t j, uint8_t *out)
{
	put_be64(out, splitmix64(SEED_DUPS + i * 16 + j));
	memcpy(out + 8, pool + val_off(i * 16 + j), VALLEN - 8);
}

/* FNV-1a over n bytes, continuing from h. */
static uint64_t fnv(uint64_t h, const void *p, size_t n)
{
	const uint8_t *b = p;
	size_t i;
	for (i = 0; i < n; i++) {
		h ^= b[i];
		h *= 0x100000001b3ULL;
	}
	return h;
}

/* The "datacheck" group: a hash of everything the generator produces for
 * n (src/data.rs computes the same; lmdb-bench --check-data compares). */
static void datacheck(size_t n)
{
	uint64_t h = 0xcbf29ce484222325ULL;
	uint8_t *k, buf[VALLEN];
	uint32_t *v;
	size_t i, j;
	k = rand_keys(n);
	h = fnv(h, k, n * KEYLEN);
	free(k);
	k = seq_keys(n);
	h = fnv(h, k, n * KEYLEN);
	free(k);
	h = fnv(h, pool, POOL_SIZE);
	v = rand_idx(S_GET, n, n);
	h = fnv(h, v, n * sizeof(*v));
	free(v);
	v = perm(S_DELETE, n);
	h = fnv(h, v, n * sizeof(*v));
	free(v);
	for (i = 0; i < 100; i++)
		for (j = 0; j < DUPS; j++) {
			dup_data(i, j, buf);
			h = fnv(h, buf, VALLEN);
		}
	printf("DATA %016llx\n", (unsigned long long)h);
}

/* ---- timing and results ------------------------------------------------ */

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

typedef struct {
	int n;
	uint64_t ns[MAXREP];
	uint64_t ops[MAXREP];
} result;

/* Records iteration it (0 is the warm-up, not recorded). */
static void record(result *r, int it, uint64_t ns, uint64_t ops)
{
	if (it == 0 || r->n >= MAXREP)
		return;
	r->ns[r->n] = ns;
	r->ops[r->n] = ops;
	r->n++;
}

static void report(const char *name, const result *r)
{
	int i;
	printf("RESULT %s", name);
	for (i = 0; i < r->n; i++)
		printf(" %llu/%llu", (unsigned long long)r->ns[i],
		    (unsigned long long)r->ops[i]);
	printf("\n");
	fflush(stdout);
}

/* ---- environment helpers ----------------------------------------------- */

static char data_path[4096], lock_path[4096];

static void reset_dir(const char *dir)
{
	if (mkdir(dir, 0755) != 0 && errno != EEXIST) {
		perror(dir);
		exit(1);
	}
	snprintf(data_path, sizeof(data_path), "%s/data.mdb", dir);
	snprintf(lock_path, sizeof(lock_path), "%s/lock.mdb", dir);
	unlink(data_path);
	unlink(lock_path);
}

static MDB_env *open_env(const char *dir, unsigned flags)
{
	MDB_env *env;
	CHK(mdb_env_create(&env));
	CHK(mdb_env_set_mapsize(env, MAPSIZE));
	CHK(mdb_env_set_maxdbs(env, 4));
	CHK(mdb_env_set_maxreaders(env, 126));
	CHK(mdb_env_open(env, dir, flags, 0644));
	return env;
}

static MDB_dbi open_dbi(MDB_env *env, const char *name, unsigned flags)
{
	MDB_txn *txn;
	MDB_dbi dbi;
	CHK(mdb_txn_begin(env, NULL, 0, &txn));
	CHK(mdb_dbi_open(txn, name, flags | MDB_CREATE, &dbi));
	CHK(mdb_txn_commit(txn));
	return dbi;
}

/* Puts keys[order[j]] (or keys[j]) for j < n, vlen-byte values from the
 * pool at val_off(i + vsalt), committing every batch puts. */
static void put_keys(MDB_env *env, MDB_dbi dbi, const uint8_t *keys,
    const uint32_t *order, size_t n, size_t vlen, uint64_t vsalt,
    unsigned flags, size_t batch)
{
	MDB_txn *txn = NULL;
	size_t j;
	for (j = 0; j < n; j++) {
		size_t i = order ? order[j] : j;
		MDB_val k, v;
		if (!txn)
			CHK(mdb_txn_begin(env, NULL, 0, &txn));
		k.mv_size = KEYLEN;
		k.mv_data = (void *)(keys + i * KEYLEN);
		v.mv_size = vlen;
		v.mv_data = pool + val_off(i + vsalt);
		CHK(mdb_put(txn, dbi, &k, &v, flags));
		if ((j + 1) % batch == 0) {
			CHK(mdb_txn_commit(txn));
			txn = NULL;
		}
	}
	if (txn)
		CHK(mdb_txn_commit(txn));
}

/* A fresh environment holding the n random keys (the rand_put data set). */
static MDB_env *build_rand(const char *dir, const uint8_t *keys, size_t n,
    MDB_dbi *dbi)
{
	MDB_env *env;
	reset_dir(dir);
	env = open_env(dir, MDB_NOSYNC);
	*dbi = open_dbi(env, NULL, 0);
	put_keys(env, *dbi, keys, NULL, n, VALLEN, 0, 0, BATCH);
	return env;
}

static volatile uint64_t sink;

/* ---- workload groups --------------------------------------------------- */

static void put_group(const char *name, const char *dir, size_t n, int reps,
    int seq)
{
	uint8_t *keys = seq ? seq_keys(n) : rand_keys(n);
	result r = { 0 };
	int it;
	for (it = 0; it <= reps; it++) {
		MDB_env *env;
		MDB_dbi dbi;
		uint64_t t0;
		reset_dir(dir);
		env = open_env(dir, MDB_NOSYNC);
		dbi = open_dbi(env, NULL, 0);
		t0 = now_ns();
		put_keys(env, dbi, keys, NULL, n, VALLEN, 0,
		    seq ? MDB_APPEND : 0, BATCH);
		record(&r, it, now_ns() - t0, n);
		mdb_env_close(env);
	}
	report(name, &r);
	free(keys);
}

static void get_loop(MDB_env *env, MDB_dbi dbi, const uint8_t *keys,
    const uint32_t *idx, size_t count, int touch_end, result *r, int reps)
{
	int it;
	for (it = 0; it <= reps; it++) {
		MDB_txn *txn;
		uint64_t t0, sum = 0;
		size_t j;
		CHK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		t0 = now_ns();
		for (j = 0; j < count; j++) {
			MDB_val k, v;
			k.mv_size = KEYLEN;
			k.mv_data = (void *)(keys + (size_t)idx[j] * KEYLEN);
			CHK(mdb_get(txn, dbi, &k, &v));
			sum += v.mv_size + ((uint8_t *)v.mv_data)[0];
			if (touch_end)
				sum += ((uint8_t *)v.mv_data)[v.mv_size - 1];
		}
		record(r, it, now_ns() - t0, count);
		mdb_txn_abort(txn);
		sink += sum;
	}
}

typedef struct {
	MDB_env *env;
	MDB_dbi dbi;
	const uint8_t *keys;
	const uint32_t *idx;
	size_t count;
	atomic_int *start;
	atomic_int *stop;
	uint64_t entries;
} worker;

static void *reader_thread(void *arg)
{
	worker *w = arg;
	MDB_txn *txn;
	uint64_t sum = 0;
	size_t j;
	CHK(mdb_txn_begin(w->env, NULL, MDB_RDONLY, &txn));
	while (!atomic_load(w->start))
		;
	for (j = 0; j < w->count; j++) {
		MDB_val k, v;
		k.mv_size = KEYLEN;
		k.mv_data = (void *)(w->keys + (size_t)w->idx[j] * KEYLEN);
		CHK(mdb_get(txn, w->dbi, &k, &v));
		sum += v.mv_size + ((uint8_t *)v.mv_data)[0];
	}
	mdb_txn_abort(txn);
	sink += sum;
	return NULL;
}

static void scan(MDB_env *env, MDB_dbi dbi, MDB_cursor_op op, result *r,
    int reps)
{
	int it;
	for (it = 0; it <= reps; it++) {
		MDB_txn *txn;
		MDB_cursor *cur;
		MDB_val k, v;
		uint64_t t0, cnt = 0, sum = 0;
		int rc;
		CHK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		t0 = now_ns();
		CHK(mdb_cursor_open(txn, dbi, &cur));
		while ((rc = mdb_cursor_get(cur, &k, &v, op)) == 0) {
			cnt++;
			sum += ((uint8_t *)k.mv_data)[0] + v.mv_size;
		}
		if (rc != MDB_NOTFOUND)
			CHK(rc);
		mdb_cursor_close(cur);
		record(r, it, now_ns() - t0, cnt);
		mdb_txn_abort(txn);
		sink += sum;
	}
}

static void read_group(const char *dir, size_t n, int reps)
{
	uint8_t *keys = rand_keys(n);
	uint32_t *idx = rand_idx(S_GET, n, n);
	uint32_t *ridx[READERS];
	MDB_dbi dbi;
	MDB_env *env = build_rand(dir, keys, n, &dbi);
	result r_get = { 0 }, r_fwd = { 0 }, r_bwd = { 0 }, r_rd = { 0 };
	int it, t;

	get_loop(env, dbi, keys, idx, n, 0, &r_get, reps);
	report("rand_get", &r_get);
	scan(env, dbi, MDB_NEXT, &r_fwd, reps);
	report("scan_fwd", &r_fwd);
	scan(env, dbi, MDB_PREV, &r_bwd, reps);
	report("scan_bwd", &r_bwd);

	for (t = 0; t < READERS; t++)
		ridx[t] = rand_idx(S_READER + ((uint64_t)t << 48), n, n);
	for (it = 0; it <= reps; it++) {
		pthread_t th[READERS];
		worker w[READERS];
		atomic_int start = 0;
		uint64_t t0;
		for (t = 0; t < READERS; t++) {
			memset(&w[t], 0, sizeof(w[t]));
			w[t].env = env;
			w[t].dbi = dbi;
			w[t].keys = keys;
			w[t].idx = ridx[t];
			w[t].count = n;
			w[t].start = &start;
			pthread_create(&th[t], NULL, reader_thread, &w[t]);
		}
		usleep(10000);
		t0 = now_ns();
		atomic_store(&start, 1);
		for (t = 0; t < READERS; t++)
			pthread_join(th[t], NULL);
		record(&r_rd, it, now_ns() - t0, (uint64_t)READERS * n);
	}
	report("readers8_get", &r_rd);

	for (t = 0; t < READERS; t++)
		free(ridx[t]);
	mdb_env_close(env);
	free(idx);
	free(keys);
}

static void overwrite_group(const char *dir, size_t n, int reps)
{
	uint8_t *keys = rand_keys(n);
	uint32_t *order = perm(S_OVERWRITE, n);
	MDB_dbi dbi;
	MDB_env *env = build_rand(dir, keys, n, &dbi);
	result r = { 0 };
	int it;
	for (it = 0; it <= reps; it++) {
		uint64_t t0 = now_ns();
		put_keys(env, dbi, keys, order, n, VALLEN, (uint64_t)it + 1, 0,
		    BATCH);
		record(&r, it, now_ns() - t0, n);
	}
	report("rand_overwrite", &r);
	mdb_env_close(env);
	free(order);
	free(keys);
}

static void dupsort_group(const char *dir, size_t n, int reps)
{
	size_t nk = n / DUPS, j;
	uint8_t *keys = rand_keys(nk);
	uint32_t *gi = rand_idx(S_DUP_GET, n, nk);
	uint32_t *gj = rand_idx(S_DUP_GET ^ 1, n, DUPS);
	uint32_t *order = perm(S_DUP_SCAN, nk);
	result r_put = { 0 }, r_both = { 0 }, r_next = { 0 };
	MDB_env *env = NULL;
	MDB_dbi dbi = 0;
	uint8_t buf[VALLEN];
	int it;

	for (it = 0; it <= reps; it++) {
		MDB_txn *txn = NULL;
		uint64_t t0;
		size_t i, d, puts = 0;
		if (env)
			mdb_env_close(env);
		reset_dir(dir);
		env = open_env(dir, MDB_NOSYNC);
		dbi = open_dbi(env, "dups", MDB_DUPSORT);
		t0 = now_ns();
		for (i = 0; i < nk; i++) {
			for (d = 0; d < DUPS; d++) {
				MDB_val k, v;
				if (!txn)
					CHK(mdb_txn_begin(env, NULL, 0, &txn));
				dup_data(i, d, buf);
				k.mv_size = KEYLEN;
				k.mv_data = keys + i * KEYLEN;
				v.mv_size = VALLEN;
				v.mv_data = buf;
				CHK(mdb_put(txn, dbi, &k, &v, 0));
				if (++puts % BATCH == 0) {
					CHK(mdb_txn_commit(txn));
					txn = NULL;
				}
			}
		}
		if (txn)
			CHK(mdb_txn_commit(txn));
		record(&r_put, it, now_ns() - t0, nk * DUPS);
	}
	report("dup_put", &r_put);

	for (it = 0; it <= reps; it++) {
		MDB_txn *txn;
		MDB_cursor *cur;
		uint64_t t0, sum = 0;
		CHK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		CHK(mdb_cursor_open(txn, dbi, &cur));
		t0 = now_ns();
		for (j = 0; j < n; j++) {
			MDB_val k, v;
			dup_data(gi[j], gj[j], buf);
			k.mv_size = KEYLEN;
			k.mv_data = keys + (size_t)gi[j] * KEYLEN;
			v.mv_size = VALLEN;
			v.mv_data = buf;
			CHK(mdb_cursor_get(cur, &k, &v, MDB_GET_BOTH));
			sum += v.mv_size;
		}
		record(&r_both, it, now_ns() - t0, n);
		mdb_cursor_close(cur);
		mdb_txn_abort(txn);
		sink += sum;
	}
	report("dup_get_both", &r_both);

	for (it = 0; it <= reps; it++) {
		MDB_txn *txn;
		MDB_cursor *cur;
		uint64_t t0, cnt = 0, sum = 0;
		CHK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		CHK(mdb_cursor_open(txn, dbi, &cur));
		t0 = now_ns();
		for (j = 0; j < nk; j++) {
			MDB_val k, v;
			int rc;
			k.mv_size = KEYLEN;
			k.mv_data = keys + (size_t)order[j] * KEYLEN;
			CHK(mdb_cursor_get(cur, &k, &v, MDB_SET));
			do {
				cnt++;
				sum += ((uint8_t *)v.mv_data)[0];
			} while ((rc = mdb_cursor_get(cur, &k, &v,
			    MDB_NEXT_DUP)) == 0);
			if (rc != MDB_NOTFOUND)
				CHK(rc);
		}
		record(&r_next, it, now_ns() - t0, cnt);
		mdb_cursor_close(cur);
		mdb_txn_abort(txn);
		sink += sum;
	}
	report("dup_next_dup", &r_next);

	mdb_env_close(env);
	free(order);
	free(gj);
	free(gi);
	free(keys);
}

static void overflow_group(const char *dir, size_t n, int reps)
{
	static const struct {
		size_t vlen, div;
		const char *put, *get;
	} sizes[] = {
		{ 4096, 20, "put_4k", "get_4k" },
		{ 65536, 200, "put_64k", "get_64k" },
	};
	size_t s;
	for (s = 0; s < sizeof(sizes) / sizeof(sizes[0]); s++) {
		size_t cnt = n / sizes[s].div;
		uint8_t *keys = rand_keys(cnt);
		uint32_t *idx = rand_idx(S_OVF_GET, n, cnt);
		result r_put = { 0 }, r_get = { 0 };
		MDB_env *env = NULL;
		MDB_dbi dbi = 0;
		int it;
		for (it = 0; it <= reps; it++) {
			uint64_t t0;
			if (env)
				mdb_env_close(env);
			reset_dir(dir);
			env = open_env(dir, MDB_NOSYNC);
			dbi = open_dbi(env, NULL, 0);
			t0 = now_ns();
			put_keys(env, dbi, keys, NULL, cnt, sizes[s].vlen, 0, 0,
			    OVF_BATCH);
			record(&r_put, it, now_ns() - t0, cnt);
		}
		report(sizes[s].put, &r_put);
		get_loop(env, dbi, keys, idx, n, 1, &r_get, reps);
		report(sizes[s].get, &r_get);
		mdb_env_close(env);
		free(idx);
		free(keys);
	}
}

static void delete_group(const char *dir, size_t n, int reps)
{
	uint8_t *keys = rand_keys(n);
	uint32_t *order = perm(S_DELETE, n);
	result r = { 0 };
	int it;
	for (it = 0; it <= reps; it++) {
		MDB_dbi dbi;
		MDB_env *env = build_rand(dir, keys, n, &dbi);
		MDB_txn *txn = NULL;
		uint64_t t0 = now_ns();
		size_t j;
		for (j = 0; j < n; j++) {
			MDB_val k;
			if (!txn)
				CHK(mdb_txn_begin(env, NULL, 0, &txn));
			k.mv_size = KEYLEN;
			k.mv_data = keys + (size_t)order[j] * KEYLEN;
			CHK(mdb_del(txn, dbi, &k, NULL));
			if ((j + 1) % BATCH == 0) {
				CHK(mdb_txn_commit(txn));
				txn = NULL;
			}
		}
		if (txn)
			CHK(mdb_txn_commit(txn));
		record(&r, it, now_ns() - t0, n);
		mdb_env_close(env);
	}
	report("del_rand", &r);
	free(order);
	free(keys);
}

static void small_txn_group(const char *dir, size_t n, int reps,
    size_t sync_commits)
{
	static const struct {
		const char *name;
		unsigned flags;
	} modes[] = { { "txn_nosync", MDB_NOSYNC }, { "txn_sync", 0 } };
	size_t m;
	for (m = 0; m < 2; m++) {
		size_t cnt = modes[m].flags ? n / 20 : sync_commits;
		uint8_t *keys = rand_keys(cnt);
		result r = { 0 };
		int it;
		for (it = 0; it <= reps; it++) {
			MDB_env *env;
			MDB_dbi dbi;
			uint64_t t0;
			size_t j;
			reset_dir(dir);
			env = open_env(dir, modes[m].flags);
			dbi = open_dbi(env, NULL, 0);
			t0 = now_ns();
			for (j = 0; j < cnt; j++) {
				MDB_txn *txn;
				MDB_val k, v;
				CHK(mdb_txn_begin(env, NULL, 0, &txn));
				k.mv_size = KEYLEN;
				k.mv_data = keys + j * KEYLEN;
				v.mv_size = VALLEN;
				v.mv_data = pool + val_off(j);
				CHK(mdb_put(txn, dbi, &k, &v, 0));
				CHK(mdb_txn_commit(txn));
			}
			record(&r, it, now_ns() - t0, cnt);
			mdb_env_close(env);
		}
		report(modes[m].name, &r);
		free(keys);
	}
}

static void *scanner_thread(void *arg)
{
	worker *w = arg;
	while (!atomic_load(w->start))
		;
	while (!atomic_load_explicit(w->stop, memory_order_relaxed)) {
		MDB_txn *txn;
		MDB_cursor *cur;
		MDB_val k, v;
		int rc;
		CHK(mdb_txn_begin(w->env, NULL, MDB_RDONLY, &txn));
		CHK(mdb_cursor_open(txn, w->dbi, &cur));
		while ((rc = mdb_cursor_get(cur, &k, &v, MDB_NEXT)) == 0) {
			w->entries++;
			if ((w->entries & 1023) == 0 &&
			    atomic_load_explicit(w->stop, memory_order_relaxed))
				break;
		}
		if (rc != 0 && rc != MDB_NOTFOUND)
			CHK(rc);
		mdb_cursor_close(cur);
		mdb_txn_abort(txn);
	}
	return NULL;
}

static void mixed_group(const char *dir, size_t n, int reps)
{
	uint8_t *keys = rand_keys(n);
	size_t commits = n / 2000, puts = commits * MIXED_BATCH;
	uint32_t *idx = rand_idx(S_MIXED, puts, n);
	MDB_dbi dbi;
	MDB_env *env = build_rand(dir, keys, n, &dbi);
	result r_w = { 0 }, r_r = { 0 };
	int it, t;
	for (it = 0; it <= reps; it++) {
		pthread_t th[MIXED_READERS];
		worker w[MIXED_READERS];
		atomic_int start = 0, stop = 0;
		uint64_t t0, ns, entries = 0;
		size_t c, b;
		for (t = 0; t < MIXED_READERS; t++) {
			memset(&w[t], 0, sizeof(w[t]));
			w[t].env = env;
			w[t].dbi = dbi;
			w[t].start = &start;
			w[t].stop = &stop;
			pthread_create(&th[t], NULL, scanner_thread, &w[t]);
		}
		usleep(10000);
		t0 = now_ns();
		atomic_store(&start, 1);
		for (c = 0; c < commits; c++) {
			MDB_txn *txn;
			CHK(mdb_txn_begin(env, NULL, 0, &txn));
			for (b = 0; b < MIXED_BATCH; b++) {
				size_t i = idx[c * MIXED_BATCH + b];
				MDB_val k, v;
				k.mv_size = KEYLEN;
				k.mv_data = keys + i * KEYLEN;
				v.mv_size = VALLEN;
				v.mv_data = pool + val_off(i + c + 1);
				CHK(mdb_put(txn, dbi, &k, &v, 0));
			}
			CHK(mdb_txn_commit(txn));
		}
		ns = now_ns() - t0;
		atomic_store(&stop, 1);
		for (t = 0; t < MIXED_READERS; t++) {
			pthread_join(th[t], NULL);
			entries += w[t].entries;
		}
		record(&r_w, it, ns, puts);
		record(&r_r, it, ns, entries);
	}
	report("mixed_writer", &r_w);
	report("mixed_readers", &r_r);
	mdb_env_close(env);
	free(idx);
	free(keys);
}

int main(int argc, char **argv)
{
	const char *group, *dir;
	size_t n, sync_commits;
	int reps;
	if (argc != 6) {
		fprintf(stderr,
		    "usage: cbench <group> <dir> <n> <reps> <sync_commits>\n");
		return 2;
	}
	group = argv[1];
	dir = argv[2];
	n = strtoul(argv[3], NULL, 10);
	reps = atoi(argv[4]);
	sync_commits = strtoul(argv[5], NULL, 10);
	if (n < 2000 || reps < 1 || reps > MAXREP) {
		fprintf(stderr, "cbench: need n >= 2000, 1 <= reps <= %d\n",
		    MAXREP);
		return 2;
	}
	init_pool();
	if (!strcmp(group, "seq_put"))
		put_group("seq_put_append", dir, n, reps, 1);
	else if (!strcmp(group, "rand_put"))
		put_group("rand_put", dir, n, reps, 0);
	else if (!strcmp(group, "read"))
		read_group(dir, n, reps);
	else if (!strcmp(group, "overwrite"))
		overwrite_group(dir, n, reps);
	else if (!strcmp(group, "dupsort"))
		dupsort_group(dir, n, reps);
	else if (!strcmp(group, "overflow"))
		overflow_group(dir, n, reps);
	else if (!strcmp(group, "delete"))
		delete_group(dir, n, reps);
	else if (!strcmp(group, "small_txn"))
		small_txn_group(dir, n, reps, sync_commits);
	else if (!strcmp(group, "mixed"))
		mixed_group(dir, n, reps);
	else if (!strcmp(group, "datacheck"))
		datacheck(n);
	else {
		fprintf(stderr, "cbench: unknown group %s\n", group);
		return 2;
	}
	reset_dir(dir);
	rmdir(dir);
	return 0;
}
