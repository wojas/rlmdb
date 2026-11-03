/* Checks rlmdb.h's version functions and mdb_version against librlmdb
 * (C LMDB has no rlmdb.h, so there is no C build).
 *
 *   capi_version <crate-version> <version-line> <mdb-version-string>
 *
 * The arguments are what the Rust side expects: the rlmdb crate version
 * ("0.0.0"), the line of rlmdb_version_string and the string mdb_version
 * returns. Built with rlmdb's lmdb.h
 * (LMDB 1.0's) and with LMDB 0.9's; also compiled as C++ (and linked
 * against the library), which checks the extern "C" guards of rlmdb.h.
 *
 * Prints "capi_version: N checks, F failures" and exits 1 on failures.
 */
#include <stdio.h>
#include <string.h>
#include "lmdb.h"
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

static void test_struct(const char *crate_version)
{
	const rlmdb_version_info *v = rlmdb_version();
	char buf[64];
	int ma = -1, mi = -1, pa = -1;

	CHECK(v != NULL);
	CHECK(rlmdb_version() == v);
	CHECK(v->size == sizeof(rlmdb_version_info));
	snprintf(buf, sizeof(buf), "%d.%d.%d", v->rlmdb_major, v->rlmdb_minor, v->rlmdb_patch);
	CHECK(!strcmp(buf, crate_version));
	mdb_version(&ma, &mi, &pa);
	CHECK(v->lmdb_major == ma && v->lmdb_minor == mi && v->lmdb_patch == pa);
	CHECK(v->lmdb_major == 0 && v->lmdb_minor == 9 && v->lmdb_patch == 85);
	/* Each format is that of the release of one header. */
#if MDB_VERSION_MAJOR >= 1
	CHECK(v->v10_major == MDB_VERSION_MAJOR && v->v10_minor == MDB_VERSION_MINOR &&
		v->v10_patch == MDB_VERSION_PATCH);
#else
	CHECK(v->v09_major == MDB_VERSION_MAJOR && v->v09_minor == MDB_VERSION_MINOR &&
		v->v09_patch == MDB_VERSION_PATCH);
#endif
	CHECK(v->v09_major == 0 && v->v09_minor == 9 && v->v09_patch == 37);
	CHECK(v->v10_major == 1 && v->v10_minor == 0 && v->v10_patch == 3);
}

static void test_strings(const char *line, const char *lmdb_line)
{
	const char *s = rlmdb_version_string();
	char *m = mdb_version(NULL, NULL, NULL);

	CHECK(s != NULL && !strcmp(s, line));
	CHECK(rlmdb_version_string() == s);
	CHECK(strchr(s, '\n') == NULL);
	CHECK(m != NULL && !strcmp(m, lmdb_line));
	CHECK(mdb_version(NULL, NULL, NULL) == m);
}

int main(int argc, char **argv)
{
	if (argc != 4) {
		fprintf(stderr, "usage: %s <crate-version> <version-line> <mdb-version-string>\n",
			argv[0]);
		return 2;
	}
	test_struct(argv[1]);
	test_strings(argv[2], argv[3]);
#ifdef __cplusplus
	printf("capi_version (C++, LMDB %d.%d header): %d checks, %d failures\n",
		MDB_VERSION_MAJOR, MDB_VERSION_MINOR, checks, failures);
#else
	printf("capi_version (LMDB %d.%d header): %d checks, %d failures\n",
		MDB_VERSION_MAJOR, MDB_VERSION_MINOR, checks, failures);
#endif
	return failures ? 1 : 0;
}
