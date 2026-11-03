/** @file rlmdb.h
 *	@brief rlmdb extensions to the LMDB C API
 *
 *	rlmdb is a rewrite of LMDB in Rust. Its C library (liblmdb.a,
 *	liblmdb.so, liblmdb.dylib, built from rlmdb-capi) exports the
 *	functions of the unmodified lmdb.h (LMDB 0.9.37's) and, declared here, a
 *	few of its own that C LMDB does not have, plus the LMDB 1.0 constants a
 *	program needs with a 1.0 environment. A program that calls the
 *	functions needs the rlmdb library; one that only includes lmdb.h works
 *	with either.
 *
 *	This header includes lmdb.h.
 *
 *	@section rlmdb_version_sec Which library is this?
 *	#mdb_version() reports 0.9.85 in rlmdb, a patch level above every real
 *	LMDB 0.9 release, while lmdb.h keeps the 0.9.37 #MDB_VERSION_PATCH. A
 *	run-time patch level above the header's is normal for LMDB, and
 *	#mdb_version() is what programs are told to check. #rlmdb_version()
 *	tells the rest: the rlmdb version and the C LMDB release whose file
 *	format each supported format matches.
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
 *	a later release, so programs should use the functions. Requesting the other
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
 *	@copyright See COPYRIGHT in the top-level directory of rlmdb.
 *	Redistribution and use in source and binary forms, with or without
 *	modification, are permitted only as authorized by the OpenLDAP Public
 *	License, Version 2.8 (LICENSE in the same directory, or
 *	<http://www.OpenLDAP.org/license.html>).
 */
#ifndef _RLMDB_H_
#define _RLMDB_H_

#include <stdint.h>
#include "lmdb.h"

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
	int		lmdb_major, lmdb_minor, lmdb_patch;	/**< What #mdb_version() reports: 0, 9, 85 */
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
	 * For example "rlmdb 0.0.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.37;
	 * format 1.0 = LMDB 1.0.3)". Meant for logs and bug reports: programs
	 * read the version numbers from #rlmdb_version() instead of parsing it,
	 * and opt-in features, once there are any, will be listed by name
	 * (rlmdb issue #2).
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
 * (mdb.RE/1.0, LMDB 1.0.3), so that code written for 1.0 compiles and sees
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
#ifndef MDB_REMAP_CHUNKS
	/** Don't use a single mmap, remap individual chunks (LMDB 1.0, for
	 * 32-bit builds). Not implemented: #mdb_env_open() refuses it with
	 * EINVAL. */
#define MDB_REMAP_CHUNKS	0x4000000
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

#if MDB_VERSION_MAJOR < 1
	/** Unsigned type used for mapsize, entry counts and page/transaction
	 * IDs in LMDB 1.0's lmdb.h: size_t, as everywhere rlmdb runs (1.0
	 * makes it uint64_t only with MDB_VL32, for 32-bit builds). */
typedef size_t	mdb_size_t;
#endif
#ifndef MDB_SIZE_MAX
	/** Max #mdb_size_t */
#define MDB_SIZE_MAX	SIZE_MAX
#endif
#ifndef MDB_FMT_Z
#ifdef _WIN32
#define MDB_FMT_Z	"I"
#else
	/** printf/scanf format modifier for size_t */
#define MDB_FMT_Z	"z"
#endif
#endif
#ifndef MDB_PRIy
	/** #mdb_size_t printf formats, \b t = one of [diouxX] without quotes */
#define MDB_PRIy(t)	MDB_FMT_Z #t
#endif
#ifndef MDB_SCNy
	/** #mdb_size_t scanf formats, \b t = one of [dioux] without quotes */
#define MDB_SCNy(t)	MDB_FMT_Z #t
#endif
/**	@} */

/** @defgroup rlmdb_v10_api Functions of LMDB 1.0
 *
 * The 13 functions LMDB 1.0's lmdb.h (mdb.RE/1.0, LMDB 1.0.3) declares
 * and LMDB 0.9's does not, with 1.0's prototypes and types, so that a
 * program written for LMDB 1.0 compiles with rlmdb's lmdb.h and this
 * header; every rlmdb library exports them (librlmdb and both liblmdb
 * stubs). The prototypes use size_t where 1.0's have mdb_size_t, the same
 * type on every platform rlmdb supports. A program may also include LMDB
 * 1.0's own lmdb.h before this header: the types are then that header's,
 * and the declarations here repeat its own.
 *
 * #mdb_env_set_pagesize(), #mdb_txn_flags() and #mdb_cursor_is_db() are
 * implemented. Two-phase commit, encryption, checksums, incremental
 * backups and crypto modules are not: those functions make LMDB 1.0's
 * argument checks (EINVAL), then return ENOTSUP and change nothing.
 *	@{
 */
#if MDB_VERSION_MAJOR < 1
	/** @brief A callback function used to encrypt/decrypt pages in the env
	 * (LMDB 1.0). Never called: encryption is not implemented. */
typedef int (MDB_enc_func)(const MDB_val *src, MDB_val *dst, const MDB_val *key, int encdec);

	/** @brief A callback function used to checksum pages in the env
	 * (LMDB 1.0). Never called: checksums are not implemented. */
typedef void (MDB_sum_func)(const MDB_val *src, MDB_val *dst, const MDB_val *key);

	/** @brief A function for converting a string into an encryption key
	 * (LMDB 1.0). Never called. */
typedef int (MDB_str2key_func)(const char *passwd, MDB_val *key);

	/** @brief A structure for dynamically loaded crypto modules (LMDB 1.0).
	 *
	 * rlmdb loads no modules (#mdb_modload() fails); #mdb_modsetup()
	 * reads the structure.
	 */
typedef struct MDB_crypto_funcs {
	MDB_str2key_func *mcf_str2key;
	MDB_enc_func *mcf_encfunc;
	MDB_sum_func *mcf_sumfunc;
	int mcf_keysize;	/**< The size of an encryption key, in bytes */
	int mcf_esumsize;	/**< The size of the MAC, for authenticated encryption */
	int mcf_sumsize;	/**< The size of the checksum, for plain checksums */
} MDB_crypto_funcs;

	/** @brief The function a crypto module exports to return its
	 * #MDB_crypto_funcs structure (LMDB 1.0). */
typedef MDB_crypto_funcs *(MDB_crypto_hooks)(void);
#endif

	/** @brief Set the size of DB pages in bytes (LMDB 1.0).
	 *
	 * The size defaults to the OS page size. It only applies to a new
	 * LMDB 1.0 environment; an existing one keeps the page size recorded
	 * in its file. LMDB 0.9 has no such setting: #mdb_env_open() fails with
	 * EINVAL if the environment turns out to be an LMDB 0.9 one (an
	 * existing 0.9 file, or a new environment that is not asked to be 1.0,
	 * see @ref rlmdb_format_sec). This function may only be called after
	 * #mdb_env_create() and before #mdb_env_open(). Also available as
	 * #rlmdb_env_set_pagesize().
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] size A power of two from 256 to 65536
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid size was specified, or the environment is
	 *	already open.
	 * </ul>
	 */
int  mdb_env_set_pagesize(MDB_env *env, int size);

	/** @brief Retrieve the transaction's flags (LMDB 1.0).
	 *
	 * @param[in] txn A transaction handle returned by #mdb_txn_begin()
	 * @param[out] flags Address where the flags will be returned:
	 * #MDB_RDONLY for a read-only transaction, else 0.
	 * @return A non-zero error value on failure and 0 on success. Some
	 * possible errors are:
	 * <ul>
	 *	<li>EINVAL - an invalid parameter was specified.
	 * </ul>
	 */
int  mdb_txn_flags(MDB_txn *txn, unsigned int *flags);

	/** @brief Check if the cursor is pointing to a named database record
	 * (LMDB 1.0).
	 *
	 * @param[in] cursor A cursor handle returned by #mdb_cursor_open()
	 * @return 1 if current record is a named database, 0 otherwise (also
	 * for a cursor that is not positioned or whose transaction cannot be
	 * used).
	 */
int  mdb_cursor_is_db(MDB_cursor *cursor);

	/** @brief Prepare to commit a transaction (LMDB 1.0's two-phase
	 * commit). Not implemented.
	 *
	 * The transaction is not changed: it remains an ordinary transaction
	 * that may be used, committed or aborted. (LMDB 1.0 writes its data
	 * pages and only allows #mdb_txn_commit() or #mdb_txn_abort()
	 * afterwards.)
	 * @param[in] txn A transaction handle returned by #mdb_txn_begin()
	 * @return EINVAL for a NULL transaction, else ENOTSUP.
	 */
int  mdb_txn_prepare(MDB_txn *txn);

	/** @brief Roll back the last committed transaction (LMDB 1.0's
	 * two-phase commit). Not implemented: the commit stays.
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] txnid The ID of the transaction to roll back
	 * @return EINVAL for a NULL handle or one that is not open, else
	 * ENOTSUP.
	 */
int  mdb_env_rollback(MDB_env *env, size_t txnid);

	/** @brief Set encryption on an environment (LMDB 1.0). Not implemented.
	 *
	 * The environment stays unencrypted; #mdb_env_open() refuses an
	 * encrypted LMDB 1.0 file with #MDB_ENV_ENCRYPTION.
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] func An #MDB_enc_func function
	 * @param[in] key The encryption key
	 * @param[in] size The size of authentication data in bytes, if any
	 * @return EINVAL if an argument is NULL or the environment is open
	 * (LMDB 1.0's checks), else ENOTSUP.
	 */
int  mdb_env_set_encrypt(MDB_env *env, MDB_enc_func *func, const MDB_val *key, unsigned int size);

	/** @brief Set checksums on an environment (LMDB 1.0). Not implemented.
	 *
	 * The environment stays without checksums; #mdb_env_open() refuses a
	 * checksummed LMDB 1.0 file with #MDB_BAD_CHECKSUM.
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] func An #MDB_sum_func function
	 * @param[in] size The size of computed checksum values, in bytes
	 * @return EINVAL if an argument is NULL or 0 or the environment is
	 * open (LMDB 1.0's checks), else ENOTSUP.
	 */
int  mdb_env_set_checksum(MDB_env *env, MDB_sum_func *func, unsigned int size);

	/** @brief Perform an incremental dump of an environment to a file
	 * descriptor (LMDB 1.0). Not implemented: nothing is written.
	 * @param[in] env An environment handle returned by #mdb_env_create(),
	 * opened
	 * @param[in] fd The file descriptor to write the dump to
	 * @param[in] txnid The transaction ID of a previous backup
	 * @return EINVAL for a NULL handle or one that is not open, else
	 * ENOTSUP.
	 */
int  mdb_env_incr_dumpfd(MDB_env *env, mdb_filehandle_t fd, size_t txnid);

	/** @brief Perform an incremental dump of an environment to a file
	 * (LMDB 1.0). Not implemented: no file is created.
	 * @param[in] env An environment handle returned by #mdb_env_create(),
	 * opened
	 * @param[in] path The name of the file to write the dump to
	 * @param[in] txnid The transaction ID of a previous backup
	 * @return EINVAL for a NULL argument or an environment that is not
	 * open, else ENOTSUP.
	 */
int  mdb_env_incr_dump(MDB_env *env, const char *path, size_t txnid);

	/** @brief Reload an incremental dump into an environment (LMDB 1.0).
	 * Not implemented: nothing is read.
	 * @param[in] env An environment handle returned by #mdb_env_create(),
	 * opened
	 * @param[in] fd The file descriptor to read the dump from
	 * @return EINVAL for a NULL handle or one that is not open, else
	 * ENOTSUP.
	 */
int  mdb_env_incr_loadfd(MDB_env *env, mdb_filehandle_t fd);

	/** @brief Load a dynamically loadable crypto module (LMDB 1.0).
	 * Not supported: nothing is loaded.
	 * @param[in] file The pathname of the module to load
	 * @param[in] symname The name of a symbol to resolve in the module
	 * @param[out] mcf_ptr Not changed
	 * @param[out] errmsg If not NULL, set to a static message saying that
	 * rlmdb does not support crypto modules
	 * @return NULL.
	 */
void *mdb_modload(const char *file, const char *symname,
			MDB_crypto_funcs **mcf_ptr, char **errmsg);

	/** @brief Unload a dynamically loaded module (LMDB 1.0). Does nothing:
	 * #mdb_modload() returns no handles.
	 * @param[in] handle The handle returned by #mdb_modload()
	 */
void mdb_modunload(void *handle);

	/** @brief Set an environment to use the given crypto functions (LMDB
	 * 1.0).
	 *
	 * Where LMDB 1.0 would set up checksums (the structure has a checksum
	 * function and a non-zero checksum size) or encryption (it has an
	 * encryption function and a passphrase is given) on an environment that
	 * is not open, neither of which rlmdb implements, the environment is
	 * marked so that #mdb_env_open() fails with ENOTSUP: this function
	 * cannot report an error, and the program must not get an environment
	 * without the protection it asked for. Otherwise, and for NULL
	 * arguments, nothing happens. The structure's functions are not called.
	 * @param[in] env An environment handle returned by #mdb_env_create()
	 * @param[in] mcf_ptr The crypto hooks retrieved by #mdb_modload()
	 * @param[in] passphrase The secret used to generate the encryption key
	 */
void mdb_modsetup(MDB_env *env, MDB_crypto_funcs *mcf_ptr, const char *passphrase);
/**	@} */
/**	@} */

#ifdef __cplusplus
}
#endif

#endif /* _RLMDB_H_ */
