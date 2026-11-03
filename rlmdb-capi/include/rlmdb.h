/** @file rlmdb.h
 *	@brief rlmdb extensions to the LMDB C API
 *
 *	rlmdb is a rewrite of LMDB in Rust. Its C library (librlmdb: link
 *	with -lrlmdb; built from rlmdb-capi, as are two stubs named liblmdb
 *	that stand in for C LMDB 0.9 and 1.0) has the API of the lmdb.h next
 *	to this header, LMDB 1.0's, unmodified, and exports every function of
 *	LMDB 0.9's lmdb.h too, with the same ABI. This header declares the
 *	few functions of rlmdb's own that C LMDB does not have. A program that
 *	calls them needs an rlmdb library; one that only includes lmdb.h works
 *	with C LMDB too.
 *
 *	Include it after either lmdb.h: LMDB 1.0's (rlmdb's API), or LMDB
 *	0.9's, for a program written for LMDB 0.9 that also uses these
 *	functions. It only needs #MDB_env and plain C types, which both have.
 *	Included without an lmdb.h before it, it includes the one next to it.
 *
 *	@section rlmdb_version_sec Which library is this?
 *	#mdb_version() of librlmdb reports rlmdb's own version: a program
 *	links against librlmdb explicitly. The LMDB 0.9 stub reports 0.9.85
 *	and the LMDB 1.0 stub 1.0.85, a patch level above every real release
 *	of the series each stands in for, so that a program built for C LMDB
 *	that compares the major and minor numbers with its lmdb.h's still runs
 *	on them. #rlmdb_version() tells the rest: the rlmdb version and the C
 *	LMDB release whose file format each supported format matches.
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
 *	<li>the library's: LMDB 0.9 in librlmdb and the LMDB 0.9 stub, as
 *		almost all programs are written for LMDB 0.9 today (and every C
 *		LMDB release reads its files; this may change in a later release);
 *		LMDB 1.0 in the LMDB 1.0 stub.
 *	</ol>
 *	RLMDB_DEFAULT_FORMAT is experimental: it may be removed or restricted in
 *	a later release, so programs should use the functions. Requesting the other
 *	format of an existing file makes #mdb_env_open() fail with
 *	#MDB_VERSION_MISMATCH; there is no conversion in place.
 *	#rlmdb_sniff_format() tells which format a file has.
 *
 *	In an LMDB 1.0 environment keys may be longer than 511 bytes
 *	(#mdb_env_get_maxkeysize() after the open), the page size can be chosen
 *	(LMDB 1.0's mdb_env_set_pagesize(), or #rlmdb_env_set_pagesize() with
 *	0.9's lmdb.h), and named databases are stored under their name with its
 *	terminating NUL, as C 1.0 does. Two-phase commit (mdb_txn_prepare(),
 *	mdb_env_rollback()) works in both formats. The other 1.0 features
 *	beyond its data file are not implemented: MDB_PREVSNAPSHOT, MDB_ENCRYPT
 *	and MDB_REMAP_CHUNKS give EINVAL, opening an encrypted or checksummed
 *	1.0 file fails with MDB_ENV_ENCRYPTION or MDB_BAD_CHECKSUM, and the
 *	functions of encryption, checksums, incremental backups and crypto
 *	modules return ENOTSUP (rlmdb-capi/README.md lists them). With 0.9's
 *	lmdb.h, LMDB 1.0's error codes are numbers without names;
 *	#mdb_strerror() knows their texts.
 *
 *	@copyright See COPYRIGHT in the top-level directory of rlmdb.
 *	Redistribution and use in source and binary forms, with or without
 *	modification, are permitted only as authorized by the OpenLDAP Public
 *	License, Version 2.8 (LICENSE in the same directory, or
 *	<http://www.OpenLDAP.org/license.html>).
 */
#ifndef _RLMDB_H_
#define _RLMDB_H_

/* Both LMDB releases' lmdb.h define _LMDB_H_: whichever came first stays. */
#ifndef _LMDB_H_
#include "lmdb.h"
#endif

#ifdef __cplusplus
extern "C" {
#endif

/** @defgroup rlmdb rlmdb extensions
 *	@{
 */
/** @defgroup rlmdb_version Version
 *	@{
 */
/** @brief The version information of the rlmdb library.
 *
 * Returned by #rlmdb_version(). Fields are only ever appended: a program
 * compiled against an older rlmdb.h reads the fields it knows, and one
 * compiled against a newer header checks @ref size before it reads a field
 * the library may not have.
 */
typedef struct rlmdb_version_info {
	unsigned	size;		/**< sizeof(rlmdb_version_info) of the library */
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;	/**< The rlmdb version */
	int		lmdb_major, lmdb_minor, lmdb_patch;	/**< What #mdb_version() reports: the rlmdb version in librlmdb, 0, 9, 85 in the LMDB 0.9 stub, 1, 0, 85 in the LMDB 1.0 stub */
	int		v09_major, v09_minor, v09_patch;	/**< C release the 0.9 format and API match */
	int		v10_major, v10_minor, v10_patch;	/**< C release the 1.0 format matches */
} rlmdb_version_info;

	/** @brief Return the version information of the rlmdb library.
	 *
	 * @return A pointer to a static structure, valid for the life of the
	 * process; it must not be modified or freed.
	 */
const rlmdb_version_info *rlmdb_version(void);

	/** @brief Return the version information as one line of text.
	 *
	 * For example "rlmdb 0.0.0 (mdb_version 0.0.0; format 0.9 = LMDB 0.9.37;
	 * format 1.0 = LMDB 1.0.3)" in librlmdb, whose #mdb_version() returns
	 * the same string ("mdb_version 0.9.85" and "mdb_version 1.0.85" in the
	 * stubs, which return LMDB's "LMDB 0.9.85: (rlmdb 0.0.0)" form there).
	 * Meant for logs and bug reports: programs read the version numbers
	 * from #rlmdb_version() instead of parsing it, and opt-in features, once
	 * there are any, will be listed by name (rlmdb issue #2).
	 * @return A static string, valid for the life of the process; it must not
	 * be modified or freed.
	 */
const char *rlmdb_version_string(void);
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
	 * The same as LMDB 1.0's mdb_env_set_pagesize(), under rlmdb's prefix,
	 * for programs that compile against LMDB 0.9's lmdb.h, which lacks it.
	 * The size defaults to the OS page size; it only
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
/**	@} */

#ifdef __cplusplus
}
#endif

#endif /* _RLMDB_H_ */
