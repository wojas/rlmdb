/** @file rlmdb.h
 *	@brief rlmdb extensions to the LMDB C API
 *
 *	rlmdb is a rewrite of LMDB in Rust. Its C library (liblmdb.a,
 *	liblmdb.so, liblmdb.dylib, built from rust/rlmdb-capi) exports the
 *	functions of the unmodified lmdb.h (LMDB 0.9.36's) and, declared here, a
 *	few of its own that C LMDB does not have, plus the LMDB 1.0 constants a
 *	program needs with a 1.0 environment. A program that calls the
 *	functions needs the rlmdb library; one that only includes lmdb.h works
 *	with either.
 *
 *	This header includes lmdb.h.
 *
 *	@section rlmdb_version_sec Which library is this?
 *	#mdb_version() reports 0.9.85 in rlmdb, a patch level above every real
 *	LMDB 0.9 release, while lmdb.h keeps the 0.9.36 #MDB_VERSION_PATCH. A
 *	run-time patch level above the header's is normal for LMDB, and
 *	#mdb_version() is what programs are told to check. #rlmdb_version()
 *	tells the rest: the rlmdb version, the C LMDB release whose file
 *	format each supported format matches, and a list of capabilities.
 *
 *	@section rlmdb_format_sec Data-file formats
 *	rlmdb reads and writes the data files of LMDB 0.9 (#RLMDB_FORMAT_09,
 *	MDB_DATA_VERSION 1) and of LMDB 1.0 (#RLMDB_FORMAT_10, MDB_DATA_VERSION
 *	3), each the way that C release does, with its lock file, so C 0.9 and
 *	C 1.0 processes can share an environment of their own format with
 *	rlmdb. The same lmdb.h functions work on both. #mdb_env_open() opens an
 *	existing data file in its format; a new one is created in:
 *	<ol>
 *	<li>the format requested with #rlmdb_env_set_format(), else
 *	<li>the process default of #rlmdb_set_default_format(), else
 *	<li>the format the environment variable RLMDB_DEFAULT_FORMAT names
 *		("0.9" or "1.0"; any other value is ignored), else
 *	<li>LMDB 0.9, which every C LMDB release reads.
 *	</ol>
 *	RLMDB_DEFAULT_FORMAT is experimental: it may be removed or restricted in
 *	a later release (capability "default-format-env" tells whether it is
 *	honoured), so programs should use the functions. Requesting the other
 *	format of an existing file makes #mdb_env_open() fail with
 *	#MDB_VERSION_MISMATCH; there is no conversion in place.
 *	#rlmdb_sniff_format() tells which format a file has.
 *
 *	In an LMDB 1.0 environment keys may be longer than 511 bytes
 *	(#mdb_env_get_maxkeysize() after the open), the page size can be chosen
 *	with #rlmdb_env_set_pagesize(), and named databases are stored under
 *	their name with its terminating NUL, as C 1.0 does. The 1.0 features
 *	beyond its data file are not implemented: #MDB_PREVSNAPSHOT and
 *	#MDB_ENCRYPT give EINVAL, and opening an encrypted or checksummed 1.0
 *	file fails with #MDB_ENV_ENCRYPTION or #MDB_BAD_CHECKSUM.
 *
 *	@copyright See rust/COPYRIGHT. Redistribution and use in source and
 *	binary forms, with or without modification, are permitted only as
 *	authorized by the OpenLDAP Public License, Version 2.8 (rust/LICENSE,
 *	or <http://www.OpenLDAP.org/license.html>).
 */
#ifndef _RLMDB_H_
#define _RLMDB_H_

#include "lmdb.h"

#ifdef __cplusplus
extern "C" {
#endif

/** @defgroup rlmdb rlmdb extensions
 *	@{
 */
/** @defgroup rlmdb_version Version and capabilities
 *	@{
 */
/** @brief The version and capabilities of the rlmdb library.
 *
 * Returned by #rlmdb_version(). Fields are only ever appended: a program
 * compiled against an older rlmdb.h reads the fields it knows, and one
 * compiled against a newer header checks @ref size before it reads a field
 * the library may not have.
 *
 * The v10 fields are 0, 0, 0 until the library has capability "format-1.0".
 * Capabilities are short lower-case tokens such as "format-0.9", one per
 * feature a program may want to test for; a token is added when its
 * feature lands and is not removed while the feature exists.
 */
typedef struct rlmdb_version_info {
	unsigned	size;		/**< sizeof(rlmdb_version_info) of the library */
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;	/**< The rlmdb version */
	int		lmdb_major, lmdb_minor, lmdb_patch;	/**< What #mdb_version() reports: 0, 9, 85 */
	int		v09_major, v09_minor, v09_patch;	/**< C release the 0.9 format and API match */
	int		v10_major, v10_minor, v10_patch;	/**< C release the 1.0 format matches */
	const char *const *capabilities;	/**< Capability tokens, sorted, NULL-terminated */
} rlmdb_version_info;

	/** @brief Return the version and capabilities of the rlmdb library.
	 *
	 * @return A pointer to a static structure, valid for the life of the
	 * process; it and the strings it points to must not be modified or freed.
	 */
const rlmdb_version_info *rlmdb_version(void);

	/** @brief Return the version and capabilities as one line of text.
	 *
	 * For example "rlmdb 0.1.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36;
	 * format 1.0 = LMDB 1.0.2; caps: default-format-env format-0.9
	 * format-1.0)". Meant for logs and bug reports; programs should
	 * test #rlmdb_version() or #rlmdb_has_capability() instead of parsing it.
	 * @return A static string, valid for the life of the process; it must not
	 * be modified or freed.
	 */
const char *rlmdb_version_string(void);

	/** @brief Tell whether the library has a capability.
	 *
	 * @param[in] name A capability token, for example "format-0.9"
	 * @return 1 if the library has the capability, 0 if it does not or
	 * if name is NULL.
	 */
int rlmdb_has_capability(const char *name);
/**	@} */

/** @defgroup rlmdb_format Data-file formats
 *	@{
 */
	/** No format: none requested, or no data file */
#define RLMDB_FORMAT_UNKNOWN	0
	/** The LMDB 0.9 data file (MDB_DATA_VERSION 1) */
#define RLMDB_FORMAT_09	9
	/** The LMDB 1.0 data file (MDB_DATA_VERSION 3) */
#define RLMDB_FORMAT_10	10

	/** @brief Set the data-file format of an environment.
	 *
	 * A new data file is created in this format; an existing one must
	 * have it, else #mdb_env_open() fails with #MDB_VERSION_MISMATCH.
	 * This function may only be called after #mdb_env_create() and before
	 * #mdb_env_open().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] format #RLMDB_FORMAT_09 or #RLMDB_FORMAT_10, or
	 * #RLMDB_FORMAT_UNKNOWN to remove the request again.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid format was specified, or the environment is
	 *	already open.
	 * </ul>
	 */
int rlmdb_env_set_format(MDB_env *env, unsigned format);

	/** @brief Get the data-file format of an environment.
	 *
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[out] format The format of the open environment
	 * (#RLMDB_FORMAT_09 or #RLMDB_FORMAT_10); before #mdb_env_open() the
	 * one requested with #rlmdb_env_set_format(), or #RLMDB_FORMAT_UNKNOWN.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid parameter was specified.
	 * </ul>
	 */
int rlmdb_env_get_format(MDB_env *env, unsigned *format);

	/** @brief Tell the data-file format of an environment without opening it.
	 *
	 * Reads the magic and version of the first meta page of the data file.
	 * The lock file is not touched and the file is not validated beyond
	 * that: opening it may still fail.
	 * @param[in] path The directory of the environment, or with
	 * #MDB_NOSUBDIR its data file, as given to #mdb_env_open()
	 * @param[in] flags #MDB_NOSUBDIR or 0; other flags are ignored
	 * @param[out] format #RLMDB_FORMAT_09 or #RLMDB_FORMAT_10, or
	 * #RLMDB_FORMAT_UNKNOWN if there is no data file or it is empty (opening
	 * creates a new environment). Not changed on failure.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>#MDB_INVALID - the file is not an LMDB data file.
	 *	<li>#MDB_VERSION_MISMATCH - the file has an LMDB data version
	 *	rlmdb does not handle (version 2, from prerelease builds of
	 *	lmdb-js).
	 *	<li>EACCES - the file cannot be read; other errno values likewise.
	 *	<li>EINVAL - an invalid parameter was specified.
	 * </ul>
	 */
int rlmdb_sniff_format(const char *path, unsigned flags, unsigned *format);

	/** @brief Set the format of new environments of this process.
	 *
	 * Used by #mdb_env_open() for a new data file when no format was
	 * requested with #rlmdb_env_set_format(); it takes precedence over the
	 * RLMDB_DEFAULT_FORMAT environment variable. Existing files keep their
	 * format. The setting is process-wide and may be changed at any time;
	 * it applies to environments opened afterwards.
	 * @param[in] format #RLMDB_FORMAT_09 or #RLMDB_FORMAT_10, or
	 * #RLMDB_FORMAT_UNKNOWN to remove the process default.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid format was specified.
	 * </ul>
	 */
int rlmdb_set_default_format(unsigned format);

	/** @brief Set the size of DB pages in bytes (LMDB 1.0).
	 *
	 * LMDB 1.0's mdb_env_set_pagesize(), under rlmdb's prefix because
	 * lmdb.h is LMDB 0.9's. The size defaults to the OS page size; it only
	 * applies to a new LMDB 1.0 environment (an existing one keeps the page
	 * size recorded in its file). LMDB 0.9 has no such setting:
	 * #mdb_env_open() fails with EINVAL if the environment turns out to be
	 * an LMDB 0.9 one, so request #RLMDB_FORMAT_10 too. This function may
	 * only be called after #mdb_env_create() and before #mdb_env_open().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] size A power of two from 256 to 65536
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid size was specified, or the environment is
	 *	already open.
	 * </ul>
	 */
int rlmdb_env_set_pagesize(MDB_env *env, unsigned size);
/**	@} */

/** @defgroup rlmdb_v10 Constants of LMDB 1.0
 *
 * The LMDB 1.0 values a program may meet with a 1.0 environment, under
 * their upstream names and with the values of LMDB 1.0's lmdb.h
 * (mdb.RE/1.0, LMDB 1.0.2), so that code written for 1.0 compiles and sees
 * the same numbers. Each is only defined here if no other header did.
 * #mdb_strerror() knows the error codes.
 *	@{
 */
#ifndef MDB_ENCRYPT
	/** Encrypted environment (LMDB 1.0's mdb_env_set_encrypt()). Not
	 * implemented: #mdb_env_open() and #mdb_env_set_flags() refuse it
	 * with EINVAL. */
#define MDB_ENCRYPT		0x2000U
#endif
#ifndef MDB_PREVSNAPSHOT
	/** Use the previous snapshot rather than the latest one. Not
	 * implemented: #mdb_env_open() refuses it with EINVAL, for
	 * environments of either format. */
#define MDB_PREVSNAPSHOT	0x2000000
#endif
#ifndef MDB_PROBLEM
	/** Unexpected problem - txn should abort */
#define MDB_PROBLEM		(-30779)
#endif
#ifndef MDB_BAD_CHECKSUM
	/** Page checksum incorrect (opening a 1.0 file with checksums) */
#define MDB_BAD_CHECKSUM	(-30778)
#endif
#ifndef MDB_CRYPTO_FAIL
	/** Encryption/decryption failed */
#define MDB_CRYPTO_FAIL		(-30777)
#endif
#ifndef MDB_ENV_ENCRYPTION
	/** Environment encryption mismatch (opening an encrypted 1.0 file) */
#define MDB_ENV_ENCRYPTION	(-30776)
#endif
#ifndef MDB_TXN_PENDING
	/** Transaction was already prepared */
#define MDB_TXN_PENDING		(-30775)
#endif
#ifndef MDB_CANT_ROLLBACK
	/** Environment can't rollback the last transaction */
#define MDB_CANT_ROLLBACK	(-30774)
#endif
#ifndef MDB_DBIS_BUSY
	/** Can't drop main DBI while other DBIs are open */
#define MDB_DBIS_BUSY	(-30773)
#endif
#ifndef MDB_SHORT_WRITE
	/** Write was incomplete */
#define MDB_SHORT_WRITE	(-30772)
#endif
#ifndef MDB_ENV_BUSY
	/** Env is busy, can't use previous snapshot */
#define MDB_ENV_BUSY	(-30771)
#endif
#ifndef MDB_IS_READONLY
	/** Env or txn is read-only, can't write */
#define MDB_IS_READONLY	(-30770)
#endif
#ifndef MDB_ADDR_BUSY
	/** Requested map address is unavailable */
#define MDB_ADDR_BUSY	(-30769)
#endif
/**	@} */
/**	@} */

#ifdef __cplusplus
}
#endif

#endif /* _RLMDB_H_ */
