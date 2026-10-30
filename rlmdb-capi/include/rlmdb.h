/** @file rlmdb.h
 *	@brief rlmdb extensions to the LMDB C API
 *
 *	rlmdb is a rewrite of LMDB in Rust. Its C library (liblmdb.a,
 *	liblmdb.so, liblmdb.dylib, built from rust/rlmdb-capi) exports the
 *	functions of the unmodified lmdb.h and, declared here, a few of its own
 *	that C LMDB does not have. A program that calls them needs the rlmdb
 *	library; one that only includes lmdb.h works with either.
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
	 * caps: format-0.9)". Meant for logs and bug reports; programs should
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
/**	@} */

#ifdef __cplusplus
}
#endif

#endif /* _RLMDB_H_ */
