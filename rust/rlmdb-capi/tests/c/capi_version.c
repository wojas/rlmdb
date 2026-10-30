/* Checks rlmdb.h's version functions and mdb_version against the Rust
 * library (C LMDB has no rlmdb.h, so there is no C build).
 *
 *   capi_version <crate-version> <version-line> <mdb-version-string>
 *
 * The arguments are what the Rust side expects: the rlmdb crate version
 * ("0.1.0"), the Display line of rlmdb::VersionInfo and the string
 * mdb_version returns. The program is also compiled as C++ (and linked
 * against the library), which checks the extern "C" guards of rlmdb.h.
 *
 * Prints "capi_version: N checks, F failures" and exits 1 on failures.
 */
#include <stdio.h>
#include <string.h>
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

/* Longer than any real capability list, so a missing NULL is caught. */
#define MAX_CAPS 64

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
	/* The 0.9 format is that of the shipped header's release. */
	CHECK(v->v09_major == MDB_VERSION_MAJOR && v->v09_minor == MDB_VERSION_MINOR &&
		v->v09_patch == MDB_VERSION_PATCH);
	CHECK(v->v09_major == 0 && v->v09_minor == 9 && v->v09_patch == 36);
	if (rlmdb_has_capability("format-1.0"))
		CHECK(v->v10_major == 1 && v->v10_minor == 0 && v->v10_patch == 2);
	else
		CHECK(v->v10_major == 0 && v->v10_minor == 0 && v->v10_patch == 0);
}

static void test_capabilities(void)
{
	const char *const *caps = rlmdb_version()->capabilities;
	int n, terminated = 0, sorted = 1, tokens = 1, all_found = 1, has_09 = 0;
	const char *p;

	CHECK(caps != NULL);
	for (n = 0; n < MAX_CAPS; n++) {
		if (caps[n] == NULL) {
			terminated = 1;
			break;
		}
		if (n > 0 && strcmp(caps[n - 1], caps[n]) >= 0)
			sorted = 0;
		if (!*caps[n])
			tokens = 0;
		for (p = caps[n]; *p; p++)
			if (!((*p >= 'a' && *p <= 'z') || (*p >= '0' && *p <= '9') || *p == '-' || *p == '.'))
				tokens = 0;
		if (rlmdb_has_capability(caps[n]) != 1)
			all_found = 0;
		if (!strcmp(caps[n], "format-0.9"))
			has_09 = 1;
	}
	CHECK(terminated);
	CHECK(n >= 1);
	CHECK(sorted);
	CHECK(tokens);
	CHECK(all_found);
	CHECK(has_09);

	CHECK(rlmdb_has_capability("format-0.9") == 1);
	CHECK(rlmdb_has_capability("nope") == 0);
	CHECK(rlmdb_has_capability("") == 0);
	CHECK(rlmdb_has_capability("format-0.9 ") == 0);
	CHECK(rlmdb_has_capability("FORMAT-0.9") == 0);
	CHECK(rlmdb_has_capability(NULL) == 0);
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
	test_capabilities();
	test_strings(argv[2], argv[3]);
#ifdef __cplusplus
	printf("capi_version (C++): %d checks, %d failures\n", checks, failures);
#else
	printf("capi_version: %d checks, %d failures\n", checks, failures);
#endif
	return failures ? 1 : 0;
}
