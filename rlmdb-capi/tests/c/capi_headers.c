/* rlmdb.h with either lmdb.h in front of it.
 *
 *   capi_headers
 *
 * Compiled with warnings as errors, as C and as C++, once with rlmdb's
 * lmdb.h (LMDB 0.9.37's) and once with LMDB 1.0's own lmdb.h included
 * before rlmdb.h, as a program written for C LMDB 1.0 may do: the types
 * rlmdb.h adds for the LMDB 1.0 functions must not be defined twice, and
 * its declarations of those functions must agree with 1.0's (a
 * conflicting redeclaration does not compile). The program takes the
 * address of every function rlmdb.h declares, uses the types and
 * constants, and calls some of the functions; the test links it against
 * librlmdb (0.9 header) or the LMDB 1.0 stub (1.0 header).
 *
 * Prints "capi_headers (<header>, <language>): N checks, F failures" and
 * exits 1 on failures.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include "lmdb.h"
#include "rlmdb.h"

static int checks, failures;

#define CHECK(cond) do { checks++; if (!(cond)) { failures++; \
	fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); } } while (0)

typedef void (*any_func)(void);

/* Every function of rlmdb.h: the 13 of LMDB 1.0 and rlmdb's own. */
static const any_func functions[] = {
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
	(any_func)rlmdb_version,
	(any_func)rlmdb_version_string,
	(any_func)rlmdb_env_set_format,
	(any_func)rlmdb_env_get_format,
	(any_func)rlmdb_sniff_format,
	(any_func)rlmdb_set_default_format,
	(any_func)rlmdb_env_set_pagesize,
};

/* The callback types, with their 1.0 signatures. */
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

int main(void)
{
	MDB_enc_func *encp = enc;
	MDB_sum_func *sump = sum;
	MDB_crypto_hooks *hookp = hooks;
	MDB_crypto_funcs *mcf = hookp();
	mdb_size_t count = 42;
	char buf[64];
	char *errmsg = NULL;
	unsigned flags = 7, format = 0;
	const rlmdb_version_info *info = rlmdb_version();
	int major, minor, patch;
	size_t i;
	unsigned long long remap = MDB_REMAP_CHUNKS;
	int codes[] = { MDB_PROBLEM, MDB_BAD_CHECKSUM, MDB_CRYPTO_FAIL, MDB_ENV_ENCRYPTION,
		MDB_TXN_PENDING, MDB_CANT_ROLLBACK, MDB_DBIS_BUSY, MDB_SHORT_WRITE,
		MDB_ENV_BUSY, MDB_IS_READONLY, MDB_ADDR_BUSY };

	for (i = 0; i < sizeof(functions) / sizeof(functions[0]); i++)
		CHECK(functions[i] != NULL);
	CHECK(sizeof(mdb_size_t) == sizeof(size_t));
	CHECK(MDB_SIZE_MAX == SIZE_MAX);
	/* Not passed to printf: MinGW's format checking does not know the
	 * "I" of Windows' MDB_FMT_Z (C 1.0's). */
	snprintf(buf, sizeof(buf), "%s|%s", MDB_PRIy(u), MDB_SCNy(x));
	CHECK(strcmp(buf, MDB_FMT_Z "u|" MDB_FMT_Z "x") == 0 && count == MDB_SIZE_MAX - MDB_SIZE_MAX + 42);
	CHECK(mcf->mcf_encfunc == encp && mcf->mcf_sumfunc == sump && mcf->mcf_keysize == 32);
	CHECK(remap == 0x4000000 && MDB_PREVSNAPSHOT == 0x2000000 && MDB_ENCRYPT == 0x2000);
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
	CHECK(rlmdb_sniff_format("no-such-directory", 0, &format) == 0 &&
		format == RLMDB_FORMAT_UNKNOWN);
	CHECK(rlmdb_env_set_pagesize(NULL, 4096) == EINVAL);

	/* The library: librlmdb (0.9.85) with rlmdb's lmdb.h, the 1.0 stub
	 * (1.0.85) with LMDB 1.0's. */
	mdb_version(&major, &minor, &patch);
	CHECK(major == MDB_VERSION_MAJOR && minor == MDB_VERSION_MINOR && patch == 85);
	CHECK(info->lmdb_major == major && info->lmdb_minor == minor && info->lmdb_patch == 85);
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
