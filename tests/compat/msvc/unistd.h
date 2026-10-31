/* unistd.h for MSVC, which has none: what the unmodified upstream LMDB
 * tools (mdb_stat.c, mdb_dump.c, mdb_load.c, 1.0's mdb_drop.c include
 * <unistd.h> for getopt; mdb_stat.c also uses ssize_t, which POSIX
 * declares in <sys/types.h> and MSVC nowhere) and rlmdb's C test programs
 * need from it.
 *
 * Only on the include path of MSVC builds (rlmdb/tests/common,
 * rlmdb-capi/tests/common, rlmdb-tools/tests/common, interop/build.rs);
 * MinGW has its own <unistd.h>.
 *
 * getopt behaves like MinGW-w64's (as measured on Windows 10), so that the
 * C tools act the same with both Windows toolchains and the Rust tools match
 * them (rlmdb-tools' getopt, Flavor::Mingw): the options end at the first
 * operand or at "--", as with BSD's getopt, and the messages are
 * "<argv[0]>: unknown option -- x" and
 * "<argv[0]>: option requires an argument -- x". Each program is a single
 * translation unit, so the getopt state is static to it.
 */
#ifndef RLMDB_MSVC_UNISTD_H
#define RLMDB_MSVC_UNISTD_H

#include <io.h>
#include <process.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef getpid
#define getpid()	_getpid()
#endif

/* ssize_t, as MinGW-w64 declares it (and under its guard, which
 * tests/compat/posix_compat.h uses too). */
#ifndef _SSIZE_T_DEFINED
#define _SSIZE_T_DEFINED
#include <stdint.h>
typedef intptr_t ssize_t;
#endif

static char *optarg;
static int optind = 1, opterr = 1, optopt;

/* Position inside a cluster of options ("-abc"). */
static char *getopt_place_ = "";

static __inline int
getopt(int argc, char *const argv[], const char *opts)
{
	const char *oli;

	optarg = NULL;
	if (!*getopt_place_) {
		if (optind >= argc || argv[optind][0] != '-' || !argv[optind][1])
			return -1;
		if (!strcmp(argv[optind], "--")) {
			optind++;
			return -1;
		}
		getopt_place_ = argv[optind] + 1;
	}
	optopt = (unsigned char)*getopt_place_++;
	oli = optopt == ':' ? NULL : strchr(opts, optopt);
	if (!oli) {
		if (!*getopt_place_)
			optind++;
		if (opterr && *opts != ':')
			fprintf(stderr, "%s: unknown option -- %c\n", argv[0], optopt);
		return '?';
	}
	if (oli[1] != ':') {
		if (!*getopt_place_)
			optind++;
		return optopt;
	}
	if (*getopt_place_) {
		optarg = getopt_place_;
	} else if (optind + 1 < argc) {
		optarg = argv[++optind];
	} else {
		getopt_place_ = "";
		optind++;
		if (*opts == ':')
			return ':';
		if (opterr)
			fprintf(stderr, "%s: option requires an argument -- %c\n", argv[0], optopt);
		return '?';
	}
	getopt_place_ = "";
	optind++;
	return optopt;
}

#endif /* RLMDB_MSVC_UNISTD_H */
