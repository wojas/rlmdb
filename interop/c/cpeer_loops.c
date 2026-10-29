/* cpeer_loops.c - long-running commands for cross-process concurrency tests.
 *
 * hold-reader  pins a snapshot (a read transaction) for a while: MVCC test.
 * hold-env     keeps the environment open without a read transaction.
 * hold-writer  holds the writer lock: an open write transaction.
 * writer-loop  commits many small random write transactions.
 * reader-loop  repeatedly opens read transactions and checks invariants
 *              that every committed state of writer-loop satisfies.
 *
 * writer-loop data (README.md, "writer-loop data"), in DB <db> (or main):
 *   "w%06u" (r)   -> value "r:v:len:" + filler, total len bytes, where v is
 *                    the #commits value of the writing transaction and the
 *                    filler is SplitMix64 seeded with r << 32 ^ v
 *   "#count"      -> decimal number of "w" keys
 *   "#dups"       -> decimal number of values in the dups DB
 *   "#commits"    -> decimal number of committed loop transactions
 * and in DUPSORT DB "<db>.dups" (or "dups" for the main DB):
 *   "d%04u" (r)   -> 12-byte values t:u64le, fmix32(lo(t) ^ hi(t) ^ r):u32le
 * A reader checks that every value is well formed and that the counters
 * match what it sees, in every snapshot.
 */
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <unistd.h>

#include "cpeer.h"

/* ---- hold-reader -------------------------------------------------------- */

/* Wait until `deadline`, or until a line or EOF arrives on stdin. */
static void
wait_stdin(double deadline)
{
	char buf[256];

	for (;;) {
		double left = deadline - now_seconds();
		struct timeval tv;
		fd_set fds;
		int rc;

		if (left <= 0)
			return;
		tv.tv_sec = (time_t)left;
		tv.tv_usec = (suseconds_t)((left - (double)tv.tv_sec) * 1e6);
		FD_ZERO(&fds);
		FD_SET(0, &fds);
		rc = select(1, &fds, NULL, NULL, &tv);
		if (rc > 0) {
			ssize_t n = read(0, buf, sizeof(buf));
			if (n <= 0 || memchr(buf, '\n', (size_t)n))
				return;
		}
	}
}

int
cmd_hold_reader(struct opts *o)
{
	double secs = parse_seconds(o->pos[1]), start;
	MDB_env *env;
	MDB_txn *txn;
	int rc = 0;

	if (o->npos != 2 && o->npos != 5)
		die(EXIT_USAGE, "hold-reader takes <env> <seconds> [<db|-> <count> <seed>]");
	if (o->npos == 5)
		parse_dataset_args(o, 2);
	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	start = now_seconds();
	printf("READY txnid=%zu pid=%d\n", mdb_txn_id(txn), (int)getpid());
	fflush(stdout);
	if (o->until_stdin)
		wait_stdin(start + secs);
	else
		sleep_seconds(secs);
	/* The snapshot must still show the data as of READY, no matter what
	 * other processes committed meanwhile. */
	if (o->npos == 5)
		rc = verify_in_txn(txn, &o->ds, 0);
	printf("released txnid=%zu held=%.3f\n", mdb_txn_id(txn), now_seconds() - start);
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return rc;
}

/* ---- hold-env ----------------------------------------------------------- */

/* Keep the environment open (read-only) without a read transaction. While
 * this process holds its shared lock on the lock file, later openers are
 * not exclusive and do not reset the reader table, so the slots of dead
 * processes stay until mdb_reader_check() clears them. */
int
cmd_hold_env(struct opts *o)
{
	double secs = parse_seconds(o->pos[1]), start;
	MDB_env *env;

	env = open_env(o, MDB_RDONLY);
	start = now_seconds();
	printf("READY pid=%d\n", (int)getpid());
	fflush(stdout);
	if (o->until_stdin)
		wait_stdin(start + secs);
	else
		sleep_seconds(secs);
	printf("released held=%.3f\n", now_seconds() - start);
	mdb_env_close(env);
	return 0;
}

/* ---- hold-writer -------------------------------------------------------- */

/* Begin a write transaction, put "#held" = "<pid>" into the main DB (not
 * committed), print READY and wait; then abort. A test kills this process
 * with SIGKILL while it holds the writer lock: the next writer must recover
 * the lock (robust mutexes on Linux), and "#held" must never be seen. */
int
cmd_hold_writer(struct opts *o)
{
	double secs = parse_seconds(o->pos[1]), start;
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key, data;
	char buf[32];

	env = open_env(o, 0);
	txn = begin_write(env);
	CHECK(mdb_dbi_open(txn, NULL, 0, &dbi));
	key.mv_data = "#held";
	key.mv_size = 5;
	data.mv_data = buf;
	data.mv_size = (size_t)snprintf(buf, sizeof(buf), "%d", (int)getpid());
	CHECK(mdb_put(txn, dbi, &key, &data, 0));
	start = now_seconds();
	printf("READY txnid=%zu pid=%d\n", mdb_txn_id(txn), (int)getpid());
	fflush(stdout);
	if (o->until_stdin)
		wait_stdin(start + secs);
	else
		sleep_seconds(secs);
	printf("released txnid=%zu held=%.3f\n", mdb_txn_id(txn), now_seconds() - start);
	abort_write(txn);
	mdb_env_close(env);
	return 0;
}

/* ---- shared writer-loop data -------------------------------------------- */

#define DUP_KEYS	64		/* key space of the dups DB */
#define WL_BIG		20000		/* occasional value forcing overflow pages */

static const char *
dups_db_name(const char *db, char *buf, size_t len)
{
	if (!db)
		return "dups";
	snprintf(buf, len, "%s.dups", db);
	return buf;
}

/* "r:v:len:" followed by SplitMix64 filler; total `len` bytes (len >= 40). */
static void
wl_value(uint32_t r, uint64_t v, size_t len, unsigned char *buf)
{
	uint64_t state = ((uint64_t)r << 32) ^ v;
	size_t off = (size_t)snprintf((char *)buf, len, "%u:%llu:%zu:", r,
		(unsigned long long)v, len);
	while (off < len) {
		uint64_t x = splitmix64(&state);
		size_t b;
		for (b = 0; b < 8 && off < len; b++, off++)
			buf[off] = (unsigned char)(x >> (8 * b));
	}
}

static uint32_t
dup_check(uint64_t t, uint32_t r)
{
	return fmix32((uint32_t)t ^ (uint32_t)(t >> 32) ^ r);
}

static void
dup_value(uint64_t t, uint32_t r, unsigned char *buf)
{
	uint32_t c = dup_check(t, r);
	int b;
	for (b = 0; b < 8; b++)
		buf[b] = (unsigned char)(t >> (8 * b));
	for (b = 0; b < 4; b++)
		buf[8 + b] = (unsigned char)(c >> (8 * b));
}

static uint64_t
get_counter(MDB_txn *txn, MDB_dbi dbi, const char *name)
{
	MDB_val key, data;
	char buf[32];
	int rc;

	key.mv_data = (void *)name;
	key.mv_size = strlen(name);
	rc = mdb_get(txn, dbi, &key, &data);
	if (rc == MDB_NOTFOUND)
		return 0;
	check_rc(rc, "mdb_get(counter)");
	if (data.mv_size == 0 || data.mv_size >= sizeof(buf))
		die(EXIT_MISMATCH, "counter %s has size %zu", name, data.mv_size);
	memcpy(buf, data.mv_data, data.mv_size);
	buf[data.mv_size] = '\0';
	return strtoull(buf, NULL, 10);
}

static void
put_counter(MDB_txn *txn, MDB_dbi dbi, const char *name, uint64_t v)
{
	MDB_val key, data;
	char buf[32];

	key.mv_data = (void *)name;
	key.mv_size = strlen(name);
	data.mv_data = buf;
	data.mv_size = (size_t)snprintf(buf, sizeof(buf), "%llu", (unsigned long long)v);
	CHECK(mdb_put(txn, dbi, &key, &data, 0));
}

/* ---- writer-loop -------------------------------------------------------- */

struct wloop {
	MDB_dbi		main, dups;
	uint64_t	rng;
	uint32_t	keys;
	unsigned char	*vbuf;
};

static uint32_t
rnd(struct wloop *w, uint32_t n)
{
	return (uint32_t)(splitmix64(&w->rng) % n);
}

/* A random batch of puts and deletes in `txn` (top-level or nested); the
 * counters are updated in the same transaction, so they stay consistent
 * with the data whether the transaction commits or aborts. */
static void
wl_ops(struct wloop *w, MDB_txn *txn, uint64_t version)
{
	uint64_t count = get_counter(txn, w->main, "#count");
	uint64_t ndups = get_counter(txn, w->main, "#dups");
	uint32_t nops = 1 + rnd(w, 8), op;
	MDB_cursor *cur;

	CHECK(mdb_cursor_open(txn, w->dups, &cur));
	for (op = 0; op < nops; op++) {
		uint32_t r = rnd(w, w->keys), sel = rnd(w, 10);
		char kbuf[16];
		MDB_val key, data;
		int rc;

		key.mv_data = kbuf;
		if (sel < 5) {
			/* insert or overwrite w<r> */
			size_t len = rnd(w, 40) == 0 ? WL_BIG : 40 + rnd(w, 100);
			key.mv_size = (size_t)snprintf(kbuf, sizeof(kbuf), "w%06u", r);
			wl_value(r, version, len, w->vbuf);
			data.mv_data = w->vbuf;
			data.mv_size = len;
			rc = mdb_put(txn, w->main, &key, &data, MDB_NOOVERWRITE);
			if (rc == MDB_KEYEXIST) {
				data.mv_data = w->vbuf;
				data.mv_size = len;
				rc = mdb_put(txn, w->main, &key, &data, 0);
			} else if (rc == 0) {
				count++;
			}
			check_rc(rc, "mdb_put(w)");
		} else if (sel < 7) {
			/* delete w<r> if present */
			key.mv_size = (size_t)snprintf(kbuf, sizeof(kbuf), "w%06u", r);
			rc = mdb_del(txn, w->main, &key, NULL);
			if (rc == 0)
				count--;
			else if (rc != MDB_NOTFOUND)
				check_rc(rc, "mdb_del(w)");
		} else if (sel < 9) {
			/* add a value to d<r> */
			unsigned char vb[12];
			r %= DUP_KEYS;
			key.mv_size = (size_t)snprintf(kbuf, sizeof(kbuf), "d%04u", r);
			dup_value(splitmix64(&w->rng), r, vb);
			data.mv_data = vb;
			data.mv_size = sizeof(vb);
			rc = mdb_put(txn, w->dups, &key, &data, MDB_NODUPDATA);
			if (rc == 0)
				ndups++;
			else if (rc != MDB_KEYEXIST)
				check_rc(rc, "mdb_put(d)");
		} else {
			/* delete one value of d<r>, or all of them */
			size_t c;
			r %= DUP_KEYS;
			key.mv_size = (size_t)snprintf(kbuf, sizeof(kbuf), "d%04u", r);
			rc = mdb_cursor_get(cur, &key, &data, MDB_SET);
			if (rc == MDB_NOTFOUND)
				continue;
			check_rc(rc, "mdb_cursor_get(d)");
			CHECK(mdb_cursor_count(cur, &c));
			if (rnd(w, 4) == 0) {
				CHECK(mdb_cursor_del(cur, MDB_NODUPDATA));
				ndups -= c;
			} else {
				uint32_t skip = rnd(w, (uint32_t)c);
				while (skip--)
					CHECK(mdb_cursor_get(cur, &key, &data, MDB_NEXT_DUP));
				CHECK(mdb_cursor_del(cur, 0));
				ndups--;
			}
		}
	}
	mdb_cursor_close(cur);
	put_counter(txn, w->main, "#count", count);
	put_counter(txn, w->main, "#dups", ndups);
}

int
cmd_writer_loop(struct opts *o)
{
	const char *db = strcmp(o->pos[1], "-") ? o->pos[1] : NULL;
	double deadline = now_seconds() + parse_seconds(o->pos[2]);
	unsigned long long commits = 0, aborts = 0, ncommits = 0, naborts = 0;
	uint64_t last = 0;
	struct wloop w;
	char dname[256];
	MDB_env *env;
	MDB_txn *txn;

	memset(&w, 0, sizeof(w));
	w.rng = parse_u64(o->pos[3], "seed");
	w.keys = o->keys ? o->keys : 1;
	w.vbuf = malloc(WL_BIG);
	env = open_env(o, 0);

	txn = begin_write(env);
	CHECK(mdb_dbi_open(txn, db, MDB_CREATE, &w.main));
	CHECK(mdb_dbi_open(txn, dups_db_name(db, dname, sizeof(dname)),
		MDB_CREATE | MDB_DUPSORT, &w.dups));
	commit_write(txn);

	/* Always commit at least once, so "0 seconds" initialises the DBs. */
	while (commits == 0 || (now_seconds() < deadline &&
		(!o->max_commits || commits < o->max_commits))) {
		uint64_t version;

		txn = begin_write(env);
		version = get_counter(txn, w.main, "#commits") + 1;
		wl_ops(&w, txn, version);
		if (rnd(&w, 6) == 0) {
			/* A nested transaction on top, committed or aborted. */
			MDB_txn *child;
			CHECK(mdb_txn_begin(env, txn, 0, &child));
			wl_ops(&w, child, version);
			if (rnd(&w, 2)) {
				CHECK(mdb_txn_commit(child));
				ncommits++;
			} else {
				mdb_txn_abort(child);
				naborts++;
			}
		}
		if (commits > 0 && rnd(&w, 10) == 0) {
			abort_write(txn);
			aborts++;
			continue;
		}
		put_counter(txn, w.main, "#commits", version);
		commit_write(txn);
		last = version;
		if (commits++ == 0)
			printf("READY\n");
	}
	printf("commits=%llu aborts=%llu nested_commits=%llu nested_aborts=%llu last_commit=%llu\n",
		commits, aborts, ncommits, naborts, (unsigned long long)last);
	free(w.vbuf);
	mdb_env_close(env);
	return 0;
}

/* ---- reader-loop -------------------------------------------------------- */

static int
rl_fail(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

static int
rl_fail(const char *fmt, ...)
{
	va_list ap;
	fflush(stdout);
	fputs("cpeer: reader-loop: ", stderr);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	return EXIT_MISMATCH;
}

/* Parse the number in a key "<prefix><digits>" of exactly `len` bytes.
 * Keys point into the memory map and are not NUL-terminated, so copy them
 * first: sscanf() may run strlen() over its input, past the end of the map
 * (SIGBUS). */
static int
key_number(const MDB_val *key, char prefix, size_t len, unsigned int *out)
{
	char buf[16];
	char *end;

	if (key->mv_size != len || len >= sizeof(buf) || ((char *)key->mv_data)[0] != prefix)
		return 0;
	memcpy(buf, key->mv_data, len);
	buf[len] = '\0';
	*out = (unsigned int)strtoul(buf + 1, &end, 10);
	return *end == '\0';
}

/* Check one "w" record. */
static int
check_wvalue(const MDB_val *key, const MDB_val *data, unsigned char *buf)
{
	unsigned int r, kr;
	unsigned long long v;
	size_t len;
	char head[64];
	size_t n = data->mv_size < sizeof(head) - 1 ? data->mv_size : sizeof(head) - 1;

	memcpy(head, data->mv_data, n);
	head[n] = '\0';
	if (sscanf(head, "%u:%llu:%zu:", &r, &v, &len) != 3)
		return rl_fail("malformed value for key %.*s", (int)key->mv_size,
			(char *)key->mv_data);
	if (!key_number(key, 'w', 7, &kr) || kr != r)
		return rl_fail("value of key %.*s names r=%u", (int)key->mv_size,
			(char *)key->mv_data, r);
	if (len != data->mv_size || len > WL_BIG)
		return rl_fail("value of key %.*s has size %zu, header says %zu",
			(int)key->mv_size, (char *)key->mv_data, data->mv_size, len);
	wl_value(r, v, len, buf);
	if (memcmp(buf, data->mv_data, len))
		return rl_fail("value of key %.*s (v=%llu) is corrupt", (int)key->mv_size,
			(char *)key->mv_data, v);
	return 0;
}

/* Check all invariants in one snapshot; walk backwards if `reverse`. */
static int
check_snapshot(MDB_txn *txn, MDB_dbi mdbi, MDB_dbi ddbi, int reverse,
	uint64_t *commits_out, unsigned char *buf)
{
	uint64_t commits = get_counter(txn, mdbi, "#commits");
	uint64_t count = get_counter(txn, mdbi, "#count");
	uint64_t ndups = get_counter(txn, mdbi, "#dups");
	uint64_t seen = 0, dseen = 0;
	MDB_cursor *cur;
	MDB_val key, data;
	MDB_stat st;
	int rc;

	CHECK(mdb_cursor_open(txn, mdbi, &cur));
	for (rc = mdb_cursor_get(cur, &key, &data, reverse ? MDB_LAST : MDB_FIRST); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, reverse ? MDB_PREV : MDB_NEXT)) {
		if (key.mv_size != 7 || ((char *)key.mv_data)[0] != 'w')
			continue;	/* counters, named-DB records */
		if ((rc = check_wvalue(&key, &data, buf)))
			goto out;
		seen++;
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(main)");
	mdb_cursor_close(cur);
	if (seen != count)
		return rl_fail("snapshot #commits=%llu: %llu w-keys, #count says %llu",
			(unsigned long long)commits, (unsigned long long)seen,
			(unsigned long long)count);

	/* Every value of every dups key, key by key: NEXT_DUP within a key,
	 * NEXT_NODUP to the next key. */
	CHECK(mdb_cursor_open(txn, ddbi, &cur));
	for (rc = mdb_cursor_get(cur, &key, &data, MDB_FIRST); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_NODUP)) {
		size_t c, k = 0;
		unsigned int r;

		if (!key_number(&key, 'd', 5, &r)) {
			rc = rl_fail("bad dups key %.*s", (int)key.mv_size, (char *)key.mv_data);
			goto out;
		}
		CHECK(mdb_cursor_count(cur, &c));
		do {
			const unsigned char *p = data.mv_data;
			uint64_t t = 0;
			uint32_t chk = 0;
			int b;

			if (data.mv_size != 12) {
				rc = rl_fail("dup value of size %zu under d%04u", data.mv_size, r);
				goto out;
			}
			for (b = 0; b < 8; b++)
				t |= (uint64_t)p[b] << (8 * b);
			for (b = 0; b < 4; b++)
				chk |= (uint32_t)p[8 + b] << (8 * b);
			if (chk != dup_check(t, r)) {
				rc = rl_fail("corrupt dup value under d%04u", r);
				goto out;
			}
			k++;
		} while ((rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_DUP)) == 0);
		if (rc != MDB_NOTFOUND)
			check_rc(rc, "mdb_cursor_get(MDB_NEXT_DUP)");
		if (k != c) {
			rc = rl_fail("d%04u: mdb_cursor_count %zu, walked %zu", r, c, k);
			goto out;
		}
		dseen += k;
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(dups)");
	mdb_cursor_close(cur);
	CHECK(mdb_stat(txn, ddbi, &st));
	if (dseen != ndups || st.ms_entries != ndups)
		return rl_fail("snapshot #commits=%llu: %llu dup values, mdb_stat %zu, #dups says %llu",
			(unsigned long long)commits, (unsigned long long)dseen, st.ms_entries,
			(unsigned long long)ndups);
	*commits_out = commits;
	return 0;
out:
	mdb_cursor_close(cur);
	return rc;
}

int
cmd_reader_loop(struct opts *o)
{
	const char *db = strcmp(o->pos[1], "-") ? o->pos[1] : NULL;
	double deadline = now_seconds() + parse_seconds(o->pos[2]);
	unsigned long long reads = 0;
	uint64_t first = 0, last = 0;
	unsigned char *buf = malloc(WL_BIG);
	char dname[256];
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi mdbi, ddbi;
	int rc;

	env = open_env(o, MDB_RDONLY);
	/* Wait for writer-loop to create its DBs; handles opened in a read-only
	 * transaction stay valid after it commits. */
	for (;;) {
		CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		rc = mdb_dbi_open(txn, db, 0, &mdbi);
		if (!rc)
			rc = mdb_dbi_open(txn, dups_db_name(db, dname, sizeof(dname)), 0, &ddbi);
		if (!rc) {
			CHECK(mdb_txn_commit(txn));
			break;
		}
		mdb_txn_abort(txn);
		if (rc != MDB_NOTFOUND)
			check_rc(rc, "mdb_dbi_open");
		if (now_seconds() > deadline)
			die(EXIT_MISMATCH, "reader-loop: writer-loop DBs never appeared");
		sleep_seconds(0.01);
	}

	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	do {
		uint64_t commits;
		if (reads > 0) {
			/* Alternate between reset/renew and abort/begin. */
			if (reads % 2) {
				mdb_txn_reset(txn);
				CHECK(mdb_txn_renew(txn));
			} else {
				mdb_txn_abort(txn);
				CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
			}
		}
		rc = check_snapshot(txn, mdbi, ddbi, (int)(reads % 3 == 2), &commits, buf);
		if (rc)
			break;
		if (commits < last) {
			rc = rl_fail("snapshot went back in time: #commits %llu after %llu",
				(unsigned long long)commits, (unsigned long long)last);
			break;
		}
		if (reads++ == 0) {
			first = commits;
			printf("READY\n");
		}
		last = commits;
	} while (now_seconds() < deadline);
	mdb_txn_abort(txn);
	if (!rc)
		printf("reads=%llu first_commit=%llu last_commit=%llu\n", reads,
			(unsigned long long)first, (unsigned long long)last);
	free(buf);
	mdb_env_close(env);
	return rc;
}
