/* rlmdb.h after either lmdb.h, and LMDB 1.0's API from rlmdb's headers.
 *
 *   capi_headers
 *
 * Compiled with warnings as errors, as C and as C++, in three ways, and
 * linked against librlmdb:
 *
 *  - with rlmdb-capi/include alone, whose lmdb.h is LMDB 1.0's: the API
 *    of librlmdb, as a program written for LMDB 1.0 that also uses
 *    rlmdb's functions is built;
 *  - the same with RLMDB_H_ONLY defined: rlmdb.h without an lmdb.h
 *    before it, which then includes the one next to it;
 *  - with the vendored LMDB 0.9 lmdb.h (tests/lmdb-c/0.9) first on the
 *    include path: a program written for LMDB 0.9 that also uses rlmdb's
 *    functions.
 *
 * rlmdb.h must compile after either lmdb.h without defining anything
 * twice. The program takes the address of every function rlmdb.h
 * declares and, with LMDB 1.0's lmdb.h, of the 13 functions 1.0 adds,
 * uses 1.0's types and constants, and calls some of the functions.
 *
 * Prints "capi_headers (LMDB <version> header, <language>): N checks, F
 * failures" and exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#ifndef RLMDB_H_ONLY
#include "lmdb.h"
#endif
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

typedef void (*any_func)(void);

/* Every function of rlmdb.h, and with LMDB 1.0's lmdb.h the 13 it adds
 * to 0.9's. */
static const any_func functions[] = {
	(any_func)rlmdb_version,
	(any_func)rlmdb_version_string,
	(any_func)rlmdb_env_set_format,
	(any_func)rlmdb_env_get_format,
	(any_func)rlmdb_sniff_format,
	(any_func)rlmdb_set_default_format,
	(any_func)rlmdb_env_set_pagesize,
#if MDB_VERSION_MAJOR >= 1
	(any_func)mdb_env_set_pagesize,
	(any_func)mdb_txn_flags,
	(any_func)mdb_cursor_is_db,
	(any_func)mdb_txn_prepare,
	(any_func)mdb_env_rollback,
	(any_func)mdb_env_set_encrypt,
	(any_func)mdb_env_set_checksum,
	(any_func)mdb_env_incr_dumpfd,
	(any_func)mdb_env_incr_dump,
	(any_func)mdb_env_incr_loadfd,
	(any_func)mdb_modload,
	(any_func)mdb_modunload,
	(any_func)mdb_modsetup,
#endif
};

#if MDB_VERSION_MAJOR >= 1
/* The callback types of LMDB 1.0. */
static int enc(const MDB_val *src, MDB_val *dst, const MDB_val *key, int encdec)
{
	(void)src; (void)dst; (void)key; (void)encdec;
	return 0;
}

static void sum(const MDB_val *src, MDB_val *dst, const MDB_val *key)
{
	(void)src; (void)dst; (void)key;
}

static int str2key(const char *passwd, MDB_val *key)
{
	(void)passwd; (void)key;
	return 0;
}

static MDB_crypto_funcs *hooks(void)
{
	static MDB_crypto_funcs funcs = { str2key, enc, sum, 32, 0, 8 };
	return &funcs;
}

/* LMDB 1.0's types, constants and functions, with rlmdb's lmdb.h alone. */
static void test_v10_api(void)
{
	MDB_enc_func *encp = enc;
	MDB_sum_func *sump = sum;
	MDB_crypto_hooks *hookp = hooks;
	MDB_crypto_funcs *mcf = hookp();
	mdb_size_t count = 42;
	char buf[64];
	char *errmsg = NULL;
	unsigned flags = 7;
	size_t i;
	unsigned long long remap = MDB_REMAP_CHUNKS;
	int codes[] = { MDB_PROBLEM, MDB_BAD_CHECKSUM, MDB_CRYPTO_FAIL, MDB_ENV_ENCRYPTION,
		MDB_TXN_PENDING, MDB_CANT_ROLLBACK, MDB_DBIS_BUSY, MDB_SHORT_WRITE,
		MDB_ENV_BUSY, MDB_IS_READONLY, MDB_ADDR_BUSY };

	CHECK(sizeof(mdb_size_t) == sizeof(size_t));
	CHECK(MDB_SIZE_MAX == SIZE_MAX);
	/* Not passed to printf: MinGW's format checking does not know the
	 * "I" of Windows' MDB_FMT_Z. */
	snprintf(buf, sizeof(buf), "%s|%s", MDB_PRIy(u), MDB_SCNy(x));
	CHECK(strcmp(buf, MDB_FMT_Z "u|" MDB_FMT_Z "x") == 0 && count == MDB_SIZE_MAX - MDB_SIZE_MAX + 42);
	CHECK(mcf->mcf_encfunc == encp && mcf->mcf_sumfunc == sump && mcf->mcf_keysize == 32);
	CHECK(remap == 0x4000000 && MDB_PREVSNAPSHOT == 0x2000000 && MDB_ENCRYPT == 0x2000);
	CHECK(MDB_LAST_ERRCODE == MDB_ADDR_BUSY);
	for (i = 0; i < sizeof(codes) / sizeof(codes[0]); i++)
		CHECK(codes[i] == -30779 + (int)i && strncmp(mdb_strerror(codes[i]), "MDB_", 4) == 0);

	/* Calls that need no environment. */
	CHECK(mdb_env_set_pagesize(NULL, 4096) == EINVAL);
	CHECK(mdb_txn_flags(NULL, &flags) == EINVAL && flags == 7);
	CHECK(mdb_cursor_is_db(NULL) == 0);
	CHECK(mdb_txn_prepare(NULL) == EINVAL);
	CHECK(mdb_env_rollback(NULL, 1) == EINVAL);
	CHECK(mdb_env_set_encrypt(NULL, enc, NULL, 0) == EINVAL);
	CHECK(mdb_env_set_checksum(NULL, sum, 8) == EINVAL);
	CHECK(mdb_env_incr_dump(NULL, "x", 1) == EINVAL);
	CHECK(mdb_modload("no-such-module", NULL, &mcf, &errmsg) == NULL && errmsg != NULL);
	mdb_modunload(NULL);
	mdb_modsetup(NULL, mcf, NULL);
}
#endif

int main(void)
{
	const rlmdb_version_info *info = rlmdb_version();
	unsigned format = 0;
	int major = -1, minor = -1, patch = -1;
	size_t i;

	for (i = 0; i < sizeof(functions) / sizeof(functions[0]); i++)
		CHECK(functions[i] != NULL);
#if MDB_VERSION_MAJOR >= 1
	test_v10_api();
#else
	/* LMDB 1.0's codes have no names in 0.9's lmdb.h; rlmdb knows them. */
	CHECK(MDB_LAST_ERRCODE == MDB_BAD_DBI);
	CHECK(strcmp(mdb_strerror(MDB_LAST_ERRCODE + 1),
		"MDB_PROBLEM: Unexpected problem - txn should abort") == 0);
#endif
	CHECK(rlmdb_sniff_format("no-such-directory", 0, &format) == 0 &&
		format == RLMDB_FORMAT_UNKNOWN);
	CHECK(rlmdb_env_set_pagesize(NULL, 4096) == EINVAL);
	CHECK(rlmdb_env_set_format(NULL, RLMDB_FORMAT_10) == EINVAL);

	/* librlmdb reports rlmdb's version from mdb_version, whichever header
	 * the program was built with, and the same line as
	 * rlmdb_version_string. */
	mdb_version(&major, &minor, &patch);
	CHECK(major == info->rlmdb_major && minor == info->rlmdb_minor &&
		patch == info->rlmdb_patch);
	CHECK(info->lmdb_major == major && info->lmdb_minor == minor &&
		info->lmdb_patch == patch);
	CHECK(strcmp(mdb_version(NULL, NULL, NULL), rlmdb_version_string()) == 0);
	CHECK(strstr(rlmdb_version_string(), "rlmdb ") == rlmdb_version_string());

	printf("capi_headers (LMDB %d.%d.%d header, %s): %d checks, %d failures\n",
		MDB_VERSION_MAJOR, MDB_VERSION_MINOR, MDB_VERSION_PATCH,
#ifdef __cplusplus
		"C++",
#else
		"C",
#endif
		checks, failures);
	return failures ? 1 : 0;
}
