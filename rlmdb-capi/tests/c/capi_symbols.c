/* What one of rlmdb's shared libraries exports and how it behaves where
 * the three differ.
 *
 *   capi_symbols <shared-library> <scratch-dir> <names-file>
 *
 * The program links no LMDB: it loads the library with dlopen
 * (LoadLibrary on Windows) and resolves every function named in
 * <names-file> (one per line: those of LMDB 0.9's lmdb.h and of rlmdb.h),
 * printing "missing <name>" for each it cannot find. Then it
 * prints, as "R ..." lines for the test driver to compare with what the
 * library should report:
 *
 *   R version <major>.<minor>.<patch> <mdb_version string>
 *   R info <lmdb_major>.<lmdb_minor>.<lmdb_patch> (rlmdb_version())
 *   R line <rlmdb_version_string()>
 *   R default <format>     a new environment that asks for no format
 *   R request09 <format>   with rlmdb_env_set_format(RLMDB_FORMAT_09)
 *   R request10 <format>   with rlmdb_env_set_format(RLMDB_FORMAT_10)
 *   R process09 <format>   after rlmdb_set_default_format(RLMDB_FORMAT_09)
 *   R process10 <format>   after rlmdb_set_default_format(RLMDB_FORMAT_10)
 *   R explicit <format>    rlmdb_env_set_format(09) with process default 10
 *
 * where <format> is what rlmdb_sniff_format() says of the new file (9 or
 * 10); and the driver runs it again with RLMDB_DEFAULT_FORMAT set. Exits 1
 * if a name is missing or a call fails.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* mkdir, dlopen; their Windows forms (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "lmdb.h"

/* rlmdb.h's format numbers and version structure (the program includes
 * LMDB 0.9's lmdb.h only, so that it needs nothing of rlmdb to build). */
#define RLMDB_FORMAT_UNKNOWN	0
#define RLMDB_FORMAT_09	9
#define RLMDB_FORMAT_10	10
typedef struct rlmdb_version_info {
	unsigned	size;
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;
	int		lmdb_major, lmdb_minor, lmdb_patch;
	int		v09_major, v09_minor, v09_patch;
	int		v10_major, v10_minor, v10_patch;
} rlmdb_version_info;

static void *lib;
static int failures;

static void *sym(const char *name)
{
	void *p = dlsym(lib, name);
	if (!p) {
		fprintf(stderr, "dlsym %s: %s\n", name, dlerror());
		exit(1);
	}
	return p;
}

static int (*env_create)(MDB_env **);
static int (*env_open)(MDB_env *, const char *, unsigned int, mdb_mode_t);
static void (*env_close)(MDB_env *);
static char *(*strerror_)(int);
static int (*set_format)(MDB_env *, unsigned);
static int (*set_default_format)(unsigned);
static int (*sniff_format)(const char *, unsigned, unsigned *);

static const char *scratch;

/* Creates the environment <scratch>/<name>, asking for `request` if it
 * is not RLMDB_FORMAT_UNKNOWN, and prints the format of its file. */
static void new_env(const char *what, const char *name, unsigned request)
{
	char path[1200];
	MDB_env *env;
	unsigned format = 0;
	int rc;

	snprintf(path, sizeof(path), "%s/%s", scratch, name);
	mkdir(path, 0775);
	if ((rc = env_create(&env)) != 0 ||
		(request && (rc = set_format(env, request)) != 0) ||
		(rc = env_open(env, path, 0, 0664)) != 0) {
		fprintf(stderr, "%s: %s\n", what, strerror_(rc));
		failures++;
		return;
	}
	env_close(env);
	if ((rc = sniff_format(path, 0, &format)) != 0) {
		fprintf(stderr, "%s: rlmdb_sniff_format: %s\n", what, strerror_(rc));
		failures++;
	}
	printf("R %s %u\n", what, format);
}

int main(int argc, char **argv)
{
	char name[128];
	FILE *names;
	int resolved = 0, major = -1, minor = -1, patch = -1;
	char *(*version)(int *, int *, int *);
	const rlmdb_version_info *(*rlmdb_version)(void);
	const char *(*rlmdb_version_string)(void);
	const rlmdb_version_info *info;
	char *s;

	if (argc != 4) {
		fprintf(stderr, "usage: capi_symbols <shared-library> <scratch-dir> <names-file>\n");
		return 2;
	}
	scratch = argv[2];
	lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
	if (!lib) {
		fprintf(stderr, "dlopen %s: %s\n", argv[1], dlerror());
		return 1;
	}
	names = fopen(argv[3], "r");
	if (!names) {
		perror(argv[3]);
		return 1;
	}
	while (fgets(name, sizeof(name), names)) {
		name[strcspn(name, "\r\n")] = 0;
		if (!name[0])
			continue;
		if (dlsym(lib, name))
			resolved++;
		else {
			printf("missing %s\n", name);
			failures++;
		}
	}
	fclose(names);
	printf("resolved %d\n", resolved);

	*(void **)&version = sym("mdb_version");
	*(void **)&rlmdb_version = sym("rlmdb_version");
	*(void **)&rlmdb_version_string = sym("rlmdb_version_string");
	*(void **)&env_create = sym("mdb_env_create");
	*(void **)&env_open = sym("mdb_env_open");
	*(void **)&env_close = sym("mdb_env_close");
	*(void **)&strerror_ = sym("mdb_strerror");
	*(void **)&set_format = sym("rlmdb_env_set_format");
	*(void **)&set_default_format = sym("rlmdb_set_default_format");
	*(void **)&sniff_format = sym("rlmdb_sniff_format");

	s = version(&major, &minor, &patch);
	printf("R version %d.%d.%d %s\n", major, minor, patch, s);
	info = rlmdb_version();
	printf("R info %d.%d.%d\n", info->lmdb_major, info->lmdb_minor, info->lmdb_patch);
	printf("R line %s\n", rlmdb_version_string());

	new_env("default", "default", RLMDB_FORMAT_UNKNOWN);
	new_env("request09", "request09", RLMDB_FORMAT_09);
	new_env("request10", "request10", RLMDB_FORMAT_10);
	if (set_default_format(RLMDB_FORMAT_09) != 0)
		failures++;
	new_env("process09", "process09", RLMDB_FORMAT_UNKNOWN);
	if (set_default_format(RLMDB_FORMAT_10) != 0)
		failures++;
	new_env("process10", "process10", RLMDB_FORMAT_UNKNOWN);
	new_env("explicit", "explicit", RLMDB_FORMAT_09);
	if (set_default_format(RLMDB_FORMAT_UNKNOWN) != 0)
		failures++;
	new_env("reset", "reset", RLMDB_FORMAT_UNKNOWN);

	printf("capi_symbols: %d failures\n", failures);
	return failures ? 1 : 0;
}
