/* Several threads of one process each load the LMDB library with dlopen
 * and open the same environment with their own handle, read-only and
 * without a lock file, all at the same time: the pattern of a program with
 * several embedded interpreters (each loading LMDB through its binding).
 *
 *   capi_dlopen <shared-library> <environment-dir>
 *
 * The program links no LMDB: it takes every mdb_* function from the
 * library it is given with dlsym (lmdb.h only for types and constants),
 * so the same binary runs against the C library and the Rust one. Half
 * of the threads use RTLD_LOCAL, half RTLD_GLOBAL; the dynamic linker
 * hands them one copy of the library. Each thread opens, reads, closes and
 * calls dlclose; then the library is loaded and unloaded again several
 * times, and the threads run once more on the reloaded library.
 *
 * Output: "T ..." lines, which must be identical for both libraries, and
 * "U <loaded|unloaded>" lines saying whether the library was still loaded
 * after the last dlclose of each round (dlopen with RTLD_NOLOAD), which
 * may differ. Prints "capi_dlopen: N checks, F failures" and exits 1 on
 * failures.
 */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lmdb.h"

static int checks, failures;
static pthread_mutex_t count_lock = PTHREAD_MUTEX_INITIALIZER;

#define CHECK(cond) do { int ok_ = (cond); pthread_mutex_lock(&count_lock); checks++; \
	if (!ok_) { failures++; fprintf(stderr, "%s:%d: CHECK failed: %s\n", \
		__FILE__, __LINE__, #cond); } pthread_mutex_unlock(&count_lock); } while (0)

#define T(...) do { printf("T "); printf(__VA_ARGS__); printf("\n"); } while (0)

#define THREADS 4
#define RELOADS 3

static const char *libpath, *envpath;

/* The functions used, from one dlopen of the library. */
struct lmdb {
	void *lib;
	int (*env_create)(MDB_env **);
	int (*env_open)(MDB_env *, const char *, unsigned int, mdb_mode_t);
	void (*env_close)(MDB_env *);
	int (*env_info)(MDB_env *, MDB_envinfo *);
	int (*reader_list)(MDB_env *, MDB_msg_func *, void *);
	int (*txn_begin)(MDB_env *, MDB_txn *, unsigned int, MDB_txn **);
	void (*txn_abort)(MDB_txn *);
	int (*dbi_open)(MDB_txn *, const char *, unsigned int, MDB_dbi *);
	int (*cursor_open)(MDB_txn *, MDB_dbi, MDB_cursor **);
	int (*cursor_get)(MDB_cursor *, MDB_val *, MDB_val *, MDB_cursor_op);
	void (*cursor_close)(MDB_cursor *);
	char *(*strerror)(int);
	char *(*version)(int *, int *, int *);
};

static void *sym(void *lib, const char *name)
{
	void *p = dlsym(lib, name);
	if (!p) {
		fprintf(stderr, "dlsym %s: %s\n", name, dlerror());
		exit(2);
	}
	return p;
}

/* dlopen with `mode` and look up the functions. */
static void load(struct lmdb *l, int mode)
{
	l->lib = dlopen(libpath, mode);
	if (!l->lib) {
		fprintf(stderr, "dlopen %s: %s\n", libpath, dlerror());
		exit(2);
	}
	*(void **)&l->env_create = sym(l->lib, "mdb_env_create");
	*(void **)&l->env_open = sym(l->lib, "mdb_env_open");
	*(void **)&l->env_close = sym(l->lib, "mdb_env_close");
	*(void **)&l->env_info = sym(l->lib, "mdb_env_info");
	*(void **)&l->reader_list = sym(l->lib, "mdb_reader_list");
	*(void **)&l->txn_begin = sym(l->lib, "mdb_txn_begin");
	*(void **)&l->txn_abort = sym(l->lib, "mdb_txn_abort");
	*(void **)&l->dbi_open = sym(l->lib, "mdb_dbi_open");
	*(void **)&l->cursor_open = sym(l->lib, "mdb_cursor_open");
	*(void **)&l->cursor_get = sym(l->lib, "mdb_cursor_get");
	*(void **)&l->cursor_close = sym(l->lib, "mdb_cursor_close");
	*(void **)&l->strerror = sym(l->lib, "mdb_strerror");
	*(void **)&l->version = sym(l->lib, "mdb_version");
}

/* A barrier (macOS has no pthread_barrier_t). */
struct barrier {
	pthread_mutex_t m;
	pthread_cond_t c;
	int n, waiting, round;
};

static void barrier_wait(struct barrier *b)
{
	int round;
	pthread_mutex_lock(&b->m);
	round = b->round;
	if (++b->waiting == b->n) {
		b->waiting = 0;
		b->round++;
		pthread_cond_broadcast(&b->c);
	} else {
		while (round == b->round)
			pthread_cond_wait(&b->c, &b->m);
	}
	pthread_mutex_unlock(&b->m);
}

static struct barrier opened = { PTHREAD_MUTEX_INITIALIZER, PTHREAD_COND_INITIALIZER, THREADS, 0, 0 };
static struct barrier reading = { PTHREAD_MUTEX_INITIALIZER, PTHREAD_COND_INITIALIZER, THREADS, 0, 0 };

/* What a reader saw. */
struct result {
	void *lib;
	size_t entries;
	unsigned long sum;
	unsigned numreaders;
	char list[64];
};

static int collect(const char *msg, void *ctx)
{
	strncat(ctx, msg, 63 - strlen(ctx));
	return 0;
}

/* FNV-1a over the main DB's keys and values. */
static void scan(struct lmdb *l, MDB_txn *txn, struct result *r)
{
	MDB_dbi dbi;
	MDB_cursor *c;
	MDB_val k, d;
	unsigned long h = 14695981039346656037UL;
	size_t i;
	CHECK(l->dbi_open(txn, NULL, 0, &dbi) == 0);
	CHECK(l->cursor_open(txn, dbi, &c) == 0);
	r->entries = 0;
	while (l->cursor_get(c, &k, &d, MDB_NEXT) == 0) {
		for (i = 0; i < k.mv_size; i++)
			h = (h ^ ((unsigned char *)k.mv_data)[i]) * 1099511628211UL;
		for (i = 0; i < d.mv_size; i++)
			h = (h ^ ((unsigned char *)d.mv_data)[i]) * 1099511628211UL;
		r->entries++;
	}
	l->cursor_close(c);
	r->sum = h;
}

/* Opens the environment read-only without a lock file and reads it. With
 * `sync`, every thread has its handle open, then a read transaction, at
 * the same time. */
static void read_env(struct lmdb *l, struct result *r, int sync)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_envinfo info;
	CHECK(l->env_create(&env) == 0);
	CHECK(l->env_open(env, envpath, MDB_RDONLY | MDB_NOLOCK, 0664) == 0);
	if (sync)
		barrier_wait(&opened);
	CHECK(l->txn_begin(env, NULL, MDB_RDONLY, &txn) == 0);
	scan(l, txn, r);
	if (sync)
		barrier_wait(&reading);
	CHECK(l->env_info(env, &info) == 0);
	r->numreaders = info.me_numreaders;
	r->list[0] = 0;
	CHECK(l->reader_list(env, collect, r->list) == 0);
	r->list[strcspn(r->list, "\n")] = 0;
	l->txn_abort(txn);
	l->env_close(env);
}

struct job {
	int index;
	struct result r;
};

static void *reader(void *arg)
{
	struct job *j = arg;
	struct lmdb l;
	load(&l, j->index % 2 ? RTLD_NOW | RTLD_GLOBAL : RTLD_LAZY | RTLD_LOCAL);
	j->r.lib = l.lib;
	read_env(&l, &j->r, 1);
	CHECK(dlclose(l.lib) == 0);
	return NULL;
}

/* Whether the library is still loaded (without loading it). */
static void report_loaded(const char *when)
{
	void *h = dlopen(libpath, RTLD_NOW | RTLD_NOLOAD);
	printf("U %s after %s\n", h ? "loaded" : "unloaded", when);
	if (h)
		dlclose(h);
}

/* THREADS readers at once; their results must agree. */
static void run_threads(const char *name, struct result *first)
{
	pthread_t tids[THREADS];
	struct job jobs[THREADS];
	int i;
	for (i = 0; i < THREADS; i++) {
		memset(&jobs[i], 0, sizeof(jobs[i]));
		jobs[i].index = i;
		CHECK(pthread_create(&tids[i], NULL, reader, &jobs[i]) == 0);
	}
	for (i = 0; i < THREADS; i++)
		CHECK(pthread_join(tids[i], NULL) == 0);
	for (i = 0; i < THREADS; i++) {
		struct result *r = &jobs[i].r;
		/* One copy of the library, whatever the RTLD_ flags. */
		CHECK(r->lib == jobs[0].r.lib);
		CHECK(r->entries == jobs[0].r.entries && r->sum == jobs[0].r.sum);
		CHECK(r->numreaders == 0 && !strcmp(r->list, "(no reader locks)"));
		T("%s thread %d: entries %zu sum %lx numreaders %u list %s", name, i,
			r->entries, r->sum, r->numreaders, r->list);
	}
	*first = jobs[0].r;
}

int main(int argc, char **argv)
{
	struct result first, again;
	int i;
	if (argc != 3) {
		fprintf(stderr, "usage: capi_dlopen <shared-library> <environment-dir>\n");
		return 2;
	}
	libpath = argv[1];
	envpath = argv[2];
	setvbuf(stdout, NULL, _IOLBF, 0);

	run_threads("first", &first);
	CHECK(first.entries > 0);
	report_loaded("the threads");

	/* Load, use and unload the library again, from the main thread. */
	for (i = 0; i < RELOADS; i++) {
		struct lmdb l;
		struct result r;
		int major, minor, patch;
		load(&l, RTLD_NOW | RTLD_LOCAL);
		read_env(&l, &r, 0);
		CHECK(r.entries == first.entries && r.sum == first.sum);
		/* Strings the library builds on first use stay valid. */
		CHECK(l.version(&major, &minor, &patch) != NULL);
		CHECK(major == 0 && minor == 9);
		T("reload %d: entries %zu sum %lx; %s", i, r.entries, r.sum,
			l.strerror(MDB_NOTFOUND));
		CHECK(dlclose(l.lib) == 0);
		report_loaded("a reload");
	}

	run_threads("again", &again);
	CHECK(again.entries == first.entries && again.sum == first.sum);
	report_loaded("the threads again");

	printf("capi_dlopen: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
