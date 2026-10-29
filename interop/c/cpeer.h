/* cpeer.h - shared declarations of the C interop peer.
 *
 * `cpeer` implements the peer command protocol documented in
 * rust/interop/README.md using only the public LMDB API (lmdb.h), except for
 * `check-freelist`, which reads the data file directly to audit page usage.
 * A Rust peer (`rpeer`) implements the same protocol; the test harness runs
 * both against one environment and compares what they see.
 *
 * Files:
 *   cpeer.c           main(), option parsing, env open helpers
 *   dataset.c         the deterministic data set (keys, values, comparators)
 *   cpeer_data.c      fill, verify, delete, dump
 *   cpeer_loops.c     hold-reader, hold-env, writer-loop, reader-loop
 *   cpeer_misc.c      create, stat, readers, reader-check, copy, nested-test, gen
 *   cpeer_pagewalk.c  check-freelist (raw page walk of a pinned snapshot)
 */
#ifndef CPEER_H
#define CPEER_H

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#include "lmdb.h"

/* Exit codes of every command. */
#define EXIT_MISMATCH	1	/* data or invariant check failed */
#define EXIT_USAGE	2	/* bad command line */
#define EXIT_LMDB	3	/* an LMDB call failed unexpectedly */
/* 4 is "command not implemented by this peer" (rpeer before stage 3). */

/* ---- Deterministic data set (dataset.c) -------------------------------- */

/* The parameters that define a data set. fill writes it, verify checks it,
 * delete removes a deterministic subset of it. See README.md for the exact
 * definition, which the Rust peer reproduces byte for byte. */
struct dataset {
	const char	*db;		/* named DB, or NULL for the main DB */
	uint32_t	count;		/* number of keys (items i = 0..count-1) */
	uint32_t	seed;
	unsigned int	dbflags;	/* MDB_DUPSORT, MDB_INTEGERKEY, ... */
	uint32_t	valsize;	/* value size (max size for DUPSORT) */
	uint32_t	big;		/* if > 0: items with i % 3 == 0 get big values */
	uint32_t	dups;		/* max number of values per key (DUPSORT) */
	uint32_t	deleted_every;	/* 0, or the --every N of a prior delete */
};

/* Longest key the data set produces. */
#define DS_MAX_KEY	24

uint32_t fmix32(uint32_t h);
uint64_t splitmix64(uint64_t *state);

/* Fill in defaults that depend on the flags; returns 0 or an error message. */
const char *ds_finish(struct dataset *ds);
size_t ds_key(const struct dataset *ds, uint32_t i, unsigned char *buf);
uint32_t ds_ndups(const struct dataset *ds, uint32_t i);
size_t ds_vlen(const struct dataset *ds, uint32_t i, uint32_t d);
void ds_value(const struct dataset *ds, uint32_t i, uint32_t d,
	unsigned char *buf, size_t len);
/* Does item (i, d) survive `delete --every deleted_every`? */
int ds_present(const struct dataset *ds, uint32_t i, uint32_t d);
/* Does `delete --every every` delete the whole key i? */
int ds_delete_key(uint32_t every, uint32_t i);
/* Does `delete --every every` delete dup d of key i individually? */
int ds_delete_dup(const struct dataset *ds, uint32_t every, uint32_t i, uint32_t d);

MDB_cmp_func *ds_key_cmp(unsigned int dbflags);
MDB_cmp_func *ds_dup_cmp(unsigned int dbflags);

/* The expected content of a DB: items sorted by the DB's comparators. */
struct model_item {
	uint32_t	i;
	MDB_val		key;
	uint32_t	nvals;
	MDB_val		*vals;		/* sorted by the dup comparator */
};
struct model {
	struct model_item *items;
	size_t		nitems;		/* keys present */
	size_t		nentries;	/* key/value pairs present */
	uint32_t	*absent;	/* indexes of keys that must be absent */
	size_t		nabsent;
};
int model_build(const struct dataset *ds, struct model *m);
void model_free(struct model *m);

/* ---- Command line (cpeer.c) -------------------------------------------- */

#define MAX_POS	8

struct opts {
	const char	*cmd;
	const char	*pos[MAX_POS];	/* positional arguments after the command */
	int		npos;

	/* environment options, valid for every command */
	int		nosubdir, notls, nosync, nometasync;
	size_t		mapsize;	/* 0: keep the environment's size */
	unsigned int	maxdbs, maxreaders;

	/* data set and command options */
	struct dataset	ds;
	uint32_t	per_txn;	/* fill/delete: items per transaction, 0 = all */
	uint32_t	every;		/* delete --every */
	int		append, reserve, compact, all, until_stdin, check;
	uint32_t	keys;		/* writer-loop key space */
	uint64_t	max_commits;	/* writer-loop: stop after this many, 0 = no limit */
};

/* Print a message to stderr (prefixed "cpeer: ") and exit with `code`. */
void die(int code, const char *fmt, ...)
	__attribute__((format(printf, 2, 3), noreturn));
/* Exit with EXIT_LMDB if rc != 0, naming the failed call. */
void check_rc(int rc, const char *what);
#define CHECK(call)	check_rc((call), #call)

/* Top-level write transactions: die() aborts the open one. */
MDB_txn *begin_write(MDB_env *env);
void commit_write(MDB_txn *txn);
void abort_write(MDB_txn *txn);

/* Fill o->ds from the positional arguments <db|-> <count> <seed> that start
 * at o->pos[first]. */
void parse_dataset_args(struct opts *o, int first);

/* Open the environment named by o->pos[0] (path) with the global options. */
MDB_env *open_env(const struct opts *o, unsigned int extra_flags);
/* Parse an unsigned number, or die with a usage error. */
uint64_t parse_u64(const char *s, const char *what);
uint32_t parse_u32(const char *s, const char *what);
double parse_seconds(const char *s);
double now_seconds(void);
void sleep_seconds(double s);
/* Hex-encode `len` bytes to `out` (2*len+1 bytes). */
void hex_encode(const void *data, size_t len, char *out);
void print_hex(FILE *f, const void *data, size_t len);
/* The DB name as printed in outputs: "-" for the main DB. */
const char *db_label(const char *db);

/* Commands. Each returns the process exit code. */
int cmd_create(struct opts *o);
int cmd_fill(struct opts *o);
int cmd_verify(struct opts *o);
int cmd_delete(struct opts *o);
int cmd_dump(struct opts *o);
int cmd_stat(struct opts *o);
int cmd_readers(struct opts *o);
int cmd_reader_check(struct opts *o);
int cmd_hold_reader(struct opts *o);
int cmd_hold_env(struct opts *o);
int cmd_writer_loop(struct opts *o);
int cmd_reader_loop(struct opts *o);
int cmd_check_freelist(struct opts *o);
int cmd_nested_test(struct opts *o);
int cmd_copy(struct opts *o);
int cmd_gen(struct opts *o);

/* Verify the data set `ds` inside an existing transaction (shared by verify
 * and hold-reader --verify). Returns 0, or EXIT_MISMATCH after printing the
 * first difference to stderr. */
int verify_in_txn(MDB_txn *txn, const struct dataset *ds, int quiet);

#endif /* CPEER_H */
