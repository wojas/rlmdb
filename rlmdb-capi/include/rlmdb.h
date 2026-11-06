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
 *		("0.9" or "1.0"; any other value is ignored), if the environment
 *		reads the variables (@ref rlmdb_environ_sec), else
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
 *	mdb_env_rollback()) and MDB_PREVSNAPSHOT work in both formats,
 *	incremental backups (mdb_env_incr_dumpfd(), mdb_env_incr_dump(),
 *	mdb_env_incr_loadfd()) in 1.0 environments (ENOTSUP in 0.9 ones). The
 *	other 1.0 features beyond its data file are not implemented:
 *	MDB_ENCRYPT and MDB_REMAP_CHUNKS give EINVAL, opening an encrypted or
 *	checksummed 1.0 file fails with MDB_ENV_ENCRYPTION or
 *	MDB_BAD_CHECKSUM, and the functions of encryption, checksums and
 *	crypto modules return ENOTSUP (rlmdb-capi/DETAILS.md lists them). With
 *	0.9's lmdb.h, LMDB 1.0's error codes are numbers without names;
 *	#mdb_strerror() knows their texts.
 *
 *	@section rlmdb_feature_sec Opt-in features
 *	A feature is behaviour of rlmdb's own that a program, or the
 *	RLMDB_FEATURES variable, enables by name for an environment: kebab-case
 *	names that are never renamed, so that a program written for a later
 *	rlmdb runs against an earlier one (it gets #RLMDB_UNKNOWN_FEATURE for a
 *	name the library does not know). Every feature keeps the environment
 *	compatible with C LMDB, which still opens, reads and writes it, also
 *	while the feature is on in another process; what a feature writes may
 *	differ from what C writes byte for byte. In a given library a feature
 *	is off by default (#RLMDB_FEATURE_OFF), on by default
 *	(#RLMDB_FEATURE_ON) or always on (#RLMDB_FEATURE_FIXED). A feature that
 *	graduates to always on stays listed as fixed, so a program that asks
 *	for it by name keeps getting a true answer. #rlmdb_features() lists
 *	the names a library knows.
 *
 *	At #mdb_env_open() each feature starts at its default; then
 *	RLMDB_FEATURES applies, if the environment reads the variables; then
 *	what the program asked for with #rlmdb_env_enable_feature() and
 *	#rlmdb_env_disable_feature(), which wins over the variable.
 *	RLMDB_FEATURES holds names separated by commas or whitespace (any mix),
 *	a leading '-' disables a feature, and for a name given more than once
 *	the last entry wins: "RLMDB_FEATURES=a,b -c". An unknown name there
 *	makes #mdb_env_open() fail with #RLMDB_UNKNOWN_FEATURE, a '-' on a
 *	fixed feature with #RLMDB_FEATURE_IS_FIXED, so that a typo is noticed.
 *	The only feature of this release is "split-freelist", off by default:
 *	a write transaction that frees many pages saves them in free-list
 *	records of at most one page each, instead of one record that needs a
 *	contiguous run of overflow pages. Such a transaction commits under a
 *	higher transaction ID than #mdb_txn_id() reported while it was open,
 *	writing its snapshot into both meta pages under two consecutive IDs;
 *	after #mdb_txn_prepare(), #mdb_txn_id() reports the ID the commit
 *	publishes. Such a commit cannot be undone with mdb_env_rollback()
 *	(MDB_CANT_ROLLBACK). Its meta pages are written synchronously also
 *	with #MDB_NOMETASYNC. The files keep their format.
 *
 *	@section rlmdb_environ_sec Environment variables
 *	An environment handle reads rlmdb's environment variables when it is
 *	opened, unless the program turned that off with
 *	#rlmdb_env_read_environment() (an unmodified C program has no other
 *	way to control rlmdb's own behaviour; the Rust API reads none unless
 *	asked to). What the program sets with a function always wins over a
 *	variable. They are, in the order they are read:
 *	<ol>
 *	<li>RLMDB_IGNORE_ENV: if set to anything but the empty string, none
 *		of the others is read, whatever the program asked for;
 *	<li>RLMDB_FEATURES: features to enable or disable
 *		(@ref rlmdb_feature_sec);
 *	<li>RLMDB_DEFAULT_FORMAT (experimental): the format of a new
 *		environment that asks for none (@ref rlmdb_format_sec).
 *	</ol>
 *	A setuid or setgid program reads none of them (secure_getenv()
 *	semantics).
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
	 * from #rlmdb_version() instead of parsing it. The opt-in features of
	 * the library come last, "; features: a b* c!", each name followed by
	 * '*' if it is on by default and '!' if it is fixed (left out in a
	 * library without features); programs use #rlmdb_features() instead.
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

/** @defgroup rlmdb_errors Return codes
 *	rlmdb's own return codes, from -30600 downwards (-30600 to -30699 are
 *	reserved for them; LMDB's are -30799 to -30769). #mdb_strerror() knows
 *	their texts.
 *	@{
 */
	/** A feature name this library does not know */
#define RLMDB_UNKNOWN_FEATURE	(-30600)
	/** A fixed feature cannot be disabled */
#define RLMDB_FEATURE_IS_FIXED	(-30601)
/**	@} */

/** @defgroup rlmdb_features Opt-in features
 *	See @ref rlmdb_feature_sec.
 *	@{
 */
	/** Known, off unless enabled */
#define RLMDB_FEATURE_OFF	0
	/** Known, on unless disabled */
#define RLMDB_FEATURE_ON	1
	/** Known and always on: it cannot be disabled */
#define RLMDB_FEATURE_FIXED	2

	/** @brief Return the names of the features this library knows.
	 *
	 * @return A static array of static strings, ending with NULL, valid
	 * for the life of the process; it must not be modified or freed.
	 */
const char *const *rlmdb_features(void);

	/** @brief Get the default state of a feature in this library.
	 *
	 * @param[in] name The name of the feature
	 * @param[out] state #RLMDB_FEATURE_OFF, #RLMDB_FEATURE_ON or
	 * #RLMDB_FEATURE_FIXED. Not changed on failure.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>#RLMDB_UNKNOWN_FEATURE - the library does not know the name.
	 *	<li>EINVAL - an invalid parameter was specified.
	 * </ul>
	 */
int rlmdb_feature_default(const char *name, int *state);

	/** @brief Enable a feature of an environment.
	 *
	 * The feature is on in the environment whatever RLMDB_FEATURES says;
	 * this replaces an earlier #rlmdb_env_disable_feature() of it. Enabling
	 * a fixed feature does nothing. This function may only be called after
	 * #mdb_env_create() and before #mdb_env_open().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] name The name of the feature
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>#RLMDB_UNKNOWN_FEATURE - the library does not know the name.
	 *	<li>EINVAL - an invalid parameter was specified, or the environment
	 *	is already open.
	 * </ul>
	 */
int rlmdb_env_enable_feature(MDB_env *env, const char *name);

	/** @brief Disable a feature of an environment.
	 *
	 * The feature is off in the environment whatever RLMDB_FEATURES says;
	 * this replaces an earlier #rlmdb_env_enable_feature() of it. This
	 * function may only be called after #mdb_env_create() and before
	 * #mdb_env_open().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] name The name of the feature
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>#RLMDB_UNKNOWN_FEATURE - the library does not know the name.
	 *	<li>#RLMDB_FEATURE_IS_FIXED - the feature cannot be disabled.
	 *	<li>EINVAL - an invalid parameter was specified, or the environment
	 *	is already open.
	 * </ul>
	 */
int rlmdb_env_disable_feature(MDB_env *env, const char *name);

	/** @brief Tell whether a feature of an environment is on.
	 *
	 * May be called at any time. After #mdb_env_open() it reports the
	 * state the open settled on; before it, what the defaults and the
	 * program's requests give (RLMDB_FEATURES is read at the open).
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] name The name of the feature
	 * @param[out] enabled 1 if the feature is on, else 0. Not changed on
	 * failure.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>#RLMDB_UNKNOWN_FEATURE - the library does not know the name.
	 *	<li>EINVAL - an invalid parameter was specified.
	 * </ul>
	 */
int rlmdb_env_feature_enabled(MDB_env *env, const char *name, int *enabled);

	/** @brief Set whether an environment reads rlmdb's environment variables.
	 *
	 * A new handle reads them when it is opened (@ref rlmdb_environ_sec);
	 * a program that configures rlmdb itself can turn that off.
	 * RLMDB_IGNORE_ENV turns them off whatever this says. This function
	 * may only be called after #mdb_env_create() and before
	 * #mdb_env_open().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] on 0 to read none of them, anything else to read them
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid parameter was specified, or the environment
	 *	is already open.
	 * </ul>
	 */
int rlmdb_env_read_environment(MDB_env *env, int on);
/**	@} */
/**	@} */

#ifdef __cplusplus
}
#endif

#endif /* _RLMDB_H_ */
