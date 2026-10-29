/* cpeer.c - the C interop peer: command line, environment setup, helpers.
 *
 *   cpeer <command> <envpath> [args...] [options...]
 *
 * See rust/interop/README.md for the protocol: the commands, their
 * arguments, their output lines and their exit codes. Options may appear
 * anywhere after the command; everything else is positional.
 */
#include <errno.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "cpeer.h"

/* The open top-level write transaction, if any. die() aborts it so that a
 * failing peer releases the writer lock instead of leaving other processes
 * blocked on it (named semaphores, used on macOS, are not robust). */
static MDB_txn *write_txn;

MDB_txn *
begin_write(MDB_env *env)
{
	MDB_txn *txn;
	CHECK(mdb_txn_begin(env, NULL, 0, &txn));
	write_txn = txn;
	return txn;
}

void
commit_write(MDB_txn *txn)
{
	write_txn = NULL;
	CHECK(mdb_txn_commit(txn));
}

void
abort_write(MDB_txn *txn)
{
	write_txn = NULL;
	mdb_txn_abort(txn);
}

void
die(int code, const char *fmt, ...)
{
	va_list ap;

	if (write_txn) {
		MDB_txn *txn = write_txn;
		write_txn = NULL;
		mdb_txn_abort(txn);
	}
	fflush(stdout);
	fputs("cpeer: ", stderr);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	exit(code);
}

void
check_rc(int rc, const char *what)
{
	if (rc)
		die(EXIT_LMDB, "%s failed: %d %s", what, rc, mdb_strerror(rc));
}

uint64_t
parse_u64(const char *s, const char *what)
{
	char *end;
	unsigned long long v;

	if (!s || !*s)
		die(EXIT_USAGE, "missing %s", what);
	errno = 0;
	v = strtoull(s, &end, 0);
	if (errno || *end)
		die(EXIT_USAGE, "bad %s: '%s'", what, s);
	return v;
}

uint32_t
parse_u32(const char *s, const char *what)
{
	uint64_t v = parse_u64(s, what);
	if (v > UINT32_MAX)
		die(EXIT_USAGE, "%s out of range: '%s'", what, s);
	return (uint32_t)v;
}

double
parse_seconds(const char *s)
{
	char *end;
	double v;

	if (!s)
		die(EXIT_USAGE, "missing seconds");
	v = strtod(s, &end);
	if (*end || v < 0)
		die(EXIT_USAGE, "bad seconds: '%s'", s);
	return v;
}

double
now_seconds(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

void
sleep_seconds(double s)
{
	struct timespec ts;
	if (s <= 0)
		return;
	ts.tv_sec = (time_t)s;
	ts.tv_nsec = (long)((s - (double)ts.tv_sec) * 1e9);
	while (nanosleep(&ts, &ts) && errno == EINTR)
		;
}

void
hex_encode(const void *data, size_t len, char *out)
{
	static const char digits[] = "0123456789abcdef";
	const unsigned char *p = data;
	size_t k;

	for (k = 0; k < len; k++) {
		out[2 * k] = digits[p[k] >> 4];
		out[2 * k + 1] = digits[p[k] & 15];
	}
	out[2 * len] = '\0';
}

void
print_hex(FILE *f, const void *data, size_t len)
{
	const unsigned char *p = data;
	size_t k;

	for (k = 0; k < len; k++)
		fprintf(f, "%02x", p[k]);
}

const char *
db_label(const char *db)
{
	return db ? db : "-";
}

MDB_env *
open_env(const struct opts *o, unsigned int extra_flags)
{
	MDB_env *env;
	unsigned int flags = extra_flags;
	int rc;

	if (o->nosubdir)
		flags |= MDB_NOSUBDIR;
	if (o->notls)
		flags |= MDB_NOTLS;
	if (o->nosync)
		flags |= MDB_NOSYNC;
	if (o->nometasync)
		flags |= MDB_NOMETASYNC;

	CHECK(mdb_env_create(&env));
	if (o->mapsize)
		CHECK(mdb_env_set_mapsize(env, o->mapsize));
	CHECK(mdb_env_set_maxdbs(env, o->maxdbs));
	if (o->maxreaders)
		CHECK(mdb_env_set_maxreaders(env, o->maxreaders));
	rc = mdb_env_open(env, o->pos[0], flags, 0664);
	if (rc)
		die(EXIT_LMDB, "mdb_env_open(%s) failed: %d %s", o->pos[0], rc, mdb_strerror(rc));
	return env;
}

/* ---- Command table ------------------------------------------------------ */

static int cmd_commands(struct opts *o);

struct command {
	const char	*name;
	int		(*run)(struct opts *);
	int		min_pos, max_pos;	/* positional arguments incl. <env> */
	const char	*usage;
};

static const struct command commands[] = {
	{ "create", cmd_create, 3, 3, "create <env> <mapsize> <maxdbs>" },
	{ "fill", cmd_fill, 4, 4, "fill <env> <db|-> <count> <seed> [data options]" },
	{ "verify", cmd_verify, 4, 4, "verify <env> <db|-> <count> <seed> [data options]" },
	{ "delete", cmd_delete, 4, 4, "delete <env> <db|-> <count> <seed> [--every N] [data options]" },
	{ "drop", cmd_drop, 2, 2, "drop <env> <db|-> [--delete]" },
	{ "dump", cmd_dump, 1, 2, "dump <env> <db|-> | dump <env> --all" },
	{ "stat", cmd_stat, 1, 1, "stat <env>" },
	{ "readers", cmd_readers, 1, 1, "readers <env>" },
	{ "reader-check", cmd_reader_check, 1, 1, "reader-check <env>" },
	{ "hold-reader", cmd_hold_reader, 2, 5,
		"hold-reader <env> <seconds> [--until-stdin] [<db|-> <count> <seed> [data options]]" },
	{ "hold-env", cmd_hold_env, 2, 2, "hold-env <env> <seconds> [--until-stdin]" },
	{ "hold-writer", cmd_hold_writer, 2, 2, "hold-writer <env> <seconds> [--until-stdin]" },
	{ "writer-loop", cmd_writer_loop, 4, 4, "writer-loop <env> <db|-> <seconds> <seed> [--keys N] [--max-commits N]" },
	{ "reader-loop", cmd_reader_loop, 3, 3, "reader-loop <env> <db|-> <seconds>" },
	{ "check-freelist", cmd_check_freelist, 1, 1, "check-freelist <env>" },
	{ "nested-test", cmd_nested_test, 1, 1, "nested-test <env> [--check]" },
	{ "copy", cmd_copy, 2, 2, "copy <env> <dest> [--compact]" },
	{ "gen", cmd_gen, 2, 2, "gen <seed> <i> [data options]" },
	{ "commands", cmd_commands, 0, 0, "commands" },
	{ NULL, NULL, 0, 0, NULL }
};

/* "command=<name> implemented=1" for every command: the harness asks peers
 * what they implement (the Rust peer lacks the write side before stage 3). */
static int
cmd_commands(struct opts *o)
{
	const struct command *c;

	for (c = commands; c->name; c++)
		printf("command=%s implemented=1\n", c->name);
	return 0;
}

static void
usage(void)
{
	const struct command *c;

	fprintf(stderr, "usage: cpeer <command> <args...> [options]\ncommands:\n");
	for (c = commands; c->name; c++)
		fprintf(stderr, "  %s\n", c->usage);
	fprintf(stderr,
		"env options: --nosubdir --notls --nosync --nometasync --mapsize N"
		" --maxdbs N --maxreaders N\n"
		"data options: --dupsort --dupfixed --intkey --intdup --revkey --revdup"
		" --valsize N --big N --dups N --deleted-every N --per-txn N --append --reserve\n");
	exit(EXIT_USAGE);
}

/* The value of an option that takes one, i.e. argv[++*k]. */
static const char *
optval(int argc, char **argv, int *k)
{
	if (*k + 1 >= argc)
		die(EXIT_USAGE, "option %s needs a value", argv[*k]);
	return argv[++*k];
}

static void
parse_args(int argc, char **argv, struct opts *o)
{
	int k;

	memset(o, 0, sizeof(*o));
	o->maxdbs = 64;
	o->every = 2;
	o->keys = 500;
	if (argc < 2)
		usage();
	o->cmd = argv[1];
	for (k = 2; k < argc; k++) {
		const char *a = argv[k];
		if (strncmp(a, "--", 2)) {
			if (o->npos == MAX_POS)
				die(EXIT_USAGE, "too many arguments");
			o->pos[o->npos++] = a;
			continue;
		}
		/* environment options */
		if (!strcmp(a, "--nosubdir")) o->nosubdir = 1;
		else if (!strcmp(a, "--notls")) o->notls = 1;
		else if (!strcmp(a, "--nosync")) o->nosync = 1;
		else if (!strcmp(a, "--nometasync")) o->nometasync = 1;
		else if (!strcmp(a, "--mapsize")) o->mapsize = parse_u64(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--maxdbs")) o->maxdbs = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--maxreaders")) o->maxreaders = parse_u32(optval(argc, argv, &k), a);
		/* data set options */
		else if (!strcmp(a, "--dupsort")) o->ds.dbflags |= MDB_DUPSORT;
		else if (!strcmp(a, "--dupfixed")) o->ds.dbflags |= MDB_DUPSORT | MDB_DUPFIXED;
		else if (!strcmp(a, "--intkey")) o->ds.dbflags |= MDB_INTEGERKEY;
		else if (!strcmp(a, "--intdup"))
			o->ds.dbflags |= MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP;
		else if (!strcmp(a, "--revkey")) o->ds.dbflags |= MDB_REVERSEKEY;
		else if (!strcmp(a, "--revdup")) o->ds.dbflags |= MDB_DUPSORT | MDB_REVERSEDUP;
		else if (!strcmp(a, "--valsize")) o->ds.valsize = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--big")) o->ds.big = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--dups")) o->ds.dups = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--deleted-every"))
			o->ds.deleted_every = parse_u32(optval(argc, argv, &k), a);
		/* command options */
		else if (!strcmp(a, "--per-txn")) o->per_txn = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--every")) o->every = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--keys")) o->keys = parse_u32(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--max-commits"))
			o->max_commits = parse_u64(optval(argc, argv, &k), a);
		else if (!strcmp(a, "--append")) o->append = 1;
		else if (!strcmp(a, "--reserve")) o->reserve = 1;
		else if (!strcmp(a, "--compact")) o->compact = 1;
		else if (!strcmp(a, "--all")) o->all = 1;
		else if (!strcmp(a, "--until-stdin")) o->until_stdin = 1;
		else if (!strcmp(a, "--check")) o->check = 1;
		else if (!strcmp(a, "--delete")) o->del = 1;
		else die(EXIT_USAGE, "unknown option %s", a);
	}
}

void
parse_dataset_args(struct opts *o, int first)
{
	const char *err;

	o->ds.db = strcmp(o->pos[first], "-") ? o->pos[first] : NULL;
	o->ds.count = parse_u32(o->pos[first + 1], "count");
	o->ds.seed = parse_u32(o->pos[first + 2], "seed");
	if ((err = ds_finish(&o->ds)))
		die(EXIT_USAGE, "%s", err);
}

int
main(int argc, char **argv)
{
	struct opts o;
	const struct command *c;

	parse_args(argc, argv, &o);
	for (c = commands; c->name; c++) {
		if (strcmp(c->name, o.cmd))
			continue;
		if (o.npos < c->min_pos || o.npos > c->max_pos)
			die(EXIT_USAGE, "usage: cpeer %s", c->usage);
		/* Line-buffered stdout so READY lines reach the harness at once. */
		setvbuf(stdout, NULL, _IOLBF, 0);
		return c->run(&o);
	}
	if (strcmp(o.cmd, "help") && strcmp(o.cmd, "--help"))
		fprintf(stderr, "cpeer: unknown command '%s'\n", o.cmd);
	usage();
	return EXIT_USAGE;
}
