/* Checks rlmdb.h's feature functions and the environment variables of the
 * C API against librlmdb: rlmdb_features, rlmdb_feature_default,
 * rlmdb_env_enable_feature, rlmdb_env_disable_feature,
 * rlmdb_env_feature_enabled and rlmdb_env_read_environment; RLMDB_FEATURES,
 * RLMDB_IGNORE_ENV and RLMDB_DEFAULT_FORMAT as a handle reads them (by
 * default) or not (after rlmdb_env_read_environment(env, 0)); the program's
 * requests winning over the variables; the error codes and their texts;
 * the features in rlmdb_version_string. C LMDB has none of this, so there
 * is no C build. The only feature is the placeholder "test-feature", off by
 * default.
 *
 *   capi_features <scratch-dir>
 *
 * The program sets and unsets the variables in its own process. Prints
 * "capi_features: N checks, F failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* mkdir, setenv, unsetenv; their Windows forms (tests/compat). */
#include "../../../tests/compat/posix_compat.h"
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

#define RC(expr, expected) do { int rc_ = (expr), ex_ = (expected); checks++; \
	if (rc_ != ex_) { failures++; \
		fprintf(stderr, "%s:%d: %s returned %d (%s), expected %d (%s)\n", \
			__FILE__, __LINE__, #expr, rc_, mdb_strerror(rc_), ex_, mdb_strerror(ex_)); \
	} } while (0)

#define OK(expr) RC(expr, 0)

#define TEST "test-feature"

static const char *scratch;
static int ndirs;

/* A new, empty directory under the scratch directory. */
static const char *new_dir(void)
{
	static char path[8][1024];
	char *p = path[ndirs % 8];
	snprintf(p, sizeof(path[0]), "%s/env%d", scratch, ndirs++);
	if (mkdir(p, 0755) != 0) {
		fprintf(stderr, "mkdir %s: %s\n", p, strerror(errno));
		exit(2);
	}
	return p;
}

/* What rlmdb_env_feature_enabled says of TEST, -1 on failure. */
static int enabled(MDB_env *env)
{
	int on = -1;
	OK(rlmdb_env_feature_enabled(env, TEST, &on));
	return on;
}

/* How a test handle is set up before the open. */
enum { PLAIN, NO_VARS, ENABLE, DISABLE };

/* Opens a new environment set up as `setup` says; returns the open's
 * result and, if it succeeded, whether TEST is on in *on (else -1). */
static int open_with(int setup, int *on)
{
	MDB_env *env;
	int rc;
	*on = -1;
	OK(mdb_env_create(&env));
	switch (setup) {
	case NO_VARS:
		OK(rlmdb_env_read_environment(env, 0));
		break;
	case ENABLE:
		OK(rlmdb_env_enable_feature(env, TEST));
		break;
	case DISABLE:
		OK(rlmdb_env_disable_feature(env, TEST));
		break;
	}
	rc = mdb_env_open(env, new_dir(), 0, 0644);
	if (rc == 0)
		*on = enabled(env);
	mdb_env_close(env);
	return rc;
}

/* Whether TEST is on in a new environment set up as `setup` says (-1 if
 * the open failed). */
static int on_with(int setup)
{
	int on;
	OK(open_with(setup, &on));
	return on;
}

/* The format of a new environment set up as `setup` says. */
static unsigned format_with(int setup)
{
	MDB_env *env;
	unsigned f = 99;
	OK(mdb_env_create(&env));
	if (setup == NO_VARS)
		OK(rlmdb_env_read_environment(env, 0));
	OK(mdb_env_open(env, new_dir(), 0, 0644));
	OK(rlmdb_env_get_format(env, &f));
	mdb_env_close(env);
	return f;
}

static void test_list(void)
{
	const char *const *names = rlmdb_features();
	int state = 99;
	CHECK(names != NULL && names == rlmdb_features());
	CHECK(names[0] != NULL && strcmp(names[0], TEST) == 0);
	CHECK(names[1] == NULL);
	OK(rlmdb_feature_default(TEST, &state));
	CHECK(state == RLMDB_FEATURE_OFF);
	state = 99;
	RC(rlmdb_feature_default("no-such-feature", &state), RLMDB_UNKNOWN_FEATURE);
	RC(rlmdb_feature_default("", &state), RLMDB_UNKNOWN_FEATURE);
	RC(rlmdb_feature_default("Test-Feature", &state), RLMDB_UNKNOWN_FEATURE);
	RC(rlmdb_feature_default("test-feature\xff", &state), RLMDB_UNKNOWN_FEATURE);
	CHECK(state == 99);
	RC(rlmdb_feature_default(NULL, &state), EINVAL);
	RC(rlmdb_feature_default(TEST, NULL), EINVAL);
	CHECK(strcmp(mdb_strerror(RLMDB_UNKNOWN_FEATURE),
		"RLMDB_UNKNOWN_FEATURE: Unknown feature") == 0);
	CHECK(strcmp(mdb_strerror(RLMDB_FEATURE_IS_FIXED),
		"RLMDB_FEATURE_IS_FIXED: Feature cannot be disabled") == 0);
	CHECK(strstr(rlmdb_version_string(), "; features: test-feature)") != NULL);
}

static void test_requests(void)
{
	MDB_env *env;
	int on = 99;

	OK(mdb_env_create(&env));
	/* Before the open: the default, then what the program asked for. */
	CHECK(enabled(env) == 0);
	OK(rlmdb_env_enable_feature(env, TEST));
	CHECK(enabled(env) == 1);
	OK(rlmdb_env_disable_feature(env, TEST));
	CHECK(enabled(env) == 0);
	OK(rlmdb_env_enable_feature(env, TEST));
	RC(rlmdb_env_enable_feature(env, "no-such-feature"), RLMDB_UNKNOWN_FEATURE);
	RC(rlmdb_env_disable_feature(env, "no-such-feature"), RLMDB_UNKNOWN_FEATURE);
	RC(rlmdb_env_feature_enabled(env, "no-such-feature", &on), RLMDB_UNKNOWN_FEATURE);
	CHECK(on == 99);
	RC(rlmdb_env_enable_feature(env, NULL), EINVAL);
	RC(rlmdb_env_disable_feature(env, NULL), EINVAL);
	RC(rlmdb_env_feature_enabled(env, NULL, &on), EINVAL);
	RC(rlmdb_env_feature_enabled(env, TEST, NULL), EINVAL);
	RC(rlmdb_env_enable_feature(NULL, TEST), EINVAL);
	RC(rlmdb_env_disable_feature(NULL, TEST), EINVAL);
	RC(rlmdb_env_feature_enabled(NULL, TEST, &on), EINVAL);
	RC(rlmdb_env_read_environment(NULL, 1), EINVAL);
	/* Nothing changed. */
	CHECK(enabled(env) == 1);
	OK(rlmdb_env_read_environment(env, 0));
	OK(rlmdb_env_read_environment(env, 1));
	OK(mdb_env_open(env, new_dir(), 0, 0644));
	CHECK(enabled(env) == 1);
	/* After the open the settings are fixed. */
	RC(rlmdb_env_enable_feature(env, TEST), EINVAL);
	RC(rlmdb_env_disable_feature(env, TEST), EINVAL);
	RC(rlmdb_env_read_environment(env, 0), EINVAL);
	CHECK(enabled(env) == 1);
	mdb_env_close(env);

	CHECK(on_with(PLAIN) == 0);
	CHECK(on_with(ENABLE) == 1);
	CHECK(on_with(DISABLE) == 0);
}

static void test_variables(void)
{
	MDB_env *env;
	int on;

	/* RLMDB_FEATURES, read at the open by default. */
	setenv("RLMDB_FEATURES", TEST, 1);
	CHECK(on_with(PLAIN) == 1);
	CHECK(on_with(NO_VARS) == 0);
	/* Before the open the variable is not read yet. */
	OK(mdb_env_create(&env));
	CHECK(enabled(env) == 0);
	OK(mdb_env_open(env, new_dir(), 0, 0644));
	CHECK(enabled(env) == 1);
	mdb_env_close(env);
	/* The program wins. */
	CHECK(on_with(DISABLE) == 0);
	setenv("RLMDB_FEATURES", "-" TEST, 1);
	CHECK(on_with(PLAIN) == 0);
	CHECK(on_with(ENABLE) == 1);
	/* Commas and whitespace; the last entry wins. */
	setenv("RLMDB_FEATURES", " -" TEST ",\t" TEST " ", 1);
	CHECK(on_with(PLAIN) == 1);
	setenv("RLMDB_FEATURES", TEST " -" TEST, 1);
	CHECK(on_with(PLAIN) == 0);
	setenv("RLMDB_FEATURES", "", 1);
	CHECK(on_with(PLAIN) == 0);
	/* A typo fails the open, before any file is made. */
	setenv("RLMDB_FEATURES", TEST ",tset-feature", 1);
	RC(open_with(PLAIN, &on), RLMDB_UNKNOWN_FEATURE);
	RC(open_with(ENABLE, &on), RLMDB_UNKNOWN_FEATURE);
	CHECK(on_with(NO_VARS) == 0);
	{
		MDB_env *e;
		char path[1100];
		FILE *f;
		const char *dir = new_dir();
		OK(mdb_env_create(&e));
		RC(mdb_env_open(e, dir, 0, 0644), RLMDB_UNKNOWN_FEATURE);
		mdb_env_close(e);
		snprintf(path, sizeof(path), "%s/data.mdb", dir);
		f = fopen(path, "rb");
		CHECK(f == NULL);
		if (f)
			fclose(f);
	}

	/* RLMDB_IGNORE_ENV: nothing is read, whatever the program asked. */
	setenv("RLMDB_FEATURES", TEST, 1);
	setenv("RLMDB_IGNORE_ENV", "1", 1);
	CHECK(on_with(PLAIN) == 0);
	CHECK(on_with(ENABLE) == 1);
	setenv("RLMDB_FEATURES", "no-such-feature", 1);
	CHECK(on_with(PLAIN) == 0);
	setenv("RLMDB_FEATURES", TEST, 1);
	/* Empty means unset. */
	setenv("RLMDB_IGNORE_ENV", "", 1);
	CHECK(on_with(PLAIN) == 1);
	unsetenv("RLMDB_IGNORE_ENV");
	unsetenv("RLMDB_FEATURES");

	/* RLMDB_DEFAULT_FORMAT follows the same rules. */
	setenv("RLMDB_DEFAULT_FORMAT", "1.0", 1);
	CHECK(format_with(PLAIN) == RLMDB_FORMAT_10);
	CHECK(format_with(NO_VARS) == RLMDB_FORMAT_09);
	OK(mdb_env_create(&env));
	OK(rlmdb_env_read_environment(env, 0));
	/* 0.9's limit: the variable does not count before the open either. */
	CHECK(mdb_env_get_maxkeysize(env) == 511);
	OK(rlmdb_env_read_environment(env, 1));
	CHECK(mdb_env_get_maxkeysize(env) > 511);
	mdb_env_close(env);
	setenv("RLMDB_IGNORE_ENV", "yes", 1);
	CHECK(format_with(PLAIN) == RLMDB_FORMAT_09);
	unsetenv("RLMDB_IGNORE_ENV");
	unsetenv("RLMDB_DEFAULT_FORMAT");
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: capi_features <scratch-dir>\n");
		return 2;
	}
	scratch = argv[1];
	unsetenv("RLMDB_FEATURES");
	unsetenv("RLMDB_IGNORE_ENV");
	unsetenv("RLMDB_DEFAULT_FORMAT");
	test_list();
	test_requests();
	test_variables();
	printf("capi_features: %d checks, %d failures\n", checks, failures);
	return failures ? 1 : 0;
}
