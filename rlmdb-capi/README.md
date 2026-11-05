# The rlmdb C library

rlmdb is LMDB rewritten in Rust. This crate builds rlmdb's C library, a
drop-in replacement for C LMDB's `liblmdb`:

* Same API and ABI: every `mdb_*` function of LMDB 0.9 and 1.0. Programs
  compile against the original `lmdb.h`.
* Programs already linked against a shared C LMDB run on it without a
  rebuild.
* Same files: C and rlmdb processes can share an environment.
* Both data formats, LMDB 0.9 and 1.0, in one library.
* A few extra functions in `rlmdb.h`, mainly to pick the format of new
  environments.

## API documentation

For the `mdb_*` functions, use the LMDB 1.0 reference:
<http://www.lmdb.tech/doc/>. What it says applies to rlmdb, except:

* rlmdb opens both 0.9 and 1.0 files (see [Data formats](#data-formats)).
* Encryption, checksums and crypto modules are not implemented: their
  functions fail, and encrypted or checksummed files do not open.
* Incremental backups (`mdb_env_incr_dump` and friends) only work on 1.0
  environments.

## Building

```sh
cargo build --release -p librlmdb -p lmdb09-stub -p lmdb10-stub
```

This gives you three libraries in `target/release/`:

* `librlmdb` (`librlmdb.a`, `.so`, `.dylib`, `rlmdb.dll`): link new
  programs against this one with `-lrlmdb`.
* The 0.9 stub (`liblmdb09.*`): stands in for C LMDB 0.9 for programs
  that are already built.
* The 1.0 stub (`liblmdb10.*`): the same for C LMDB 1.0.

All three contain the same code; they differ in the version they report
and the format of new environments.

`scripts/install-libs.sh` builds and installs them, with the headers and
a pkg-config file:

```sh
scripts/install-libs.sh                   # into /usr/local
PREFIX=/opt/rlmdb scripts/install-libs.sh
```

The stubs end up in `lib/rlmdb/lmdb-0.9/` and `lib/rlmdb/lmdb-1.0/`,
renamed to `liblmdb`. These directories are not on the default library
path, so the stubs never replace an installed C LMDB by accident.

## Compiling a program

```sh
cc -I rlmdb-capi/include -o prog prog.c \
   -L target/release -Wl,-rpath,target/release -lrlmdb
# or, once installed:
cc -o prog prog.c $(pkg-config --cflags --libs rlmdb)
```

The `lmdb.h` in `include/` is the unmodified LMDB 1.0 header. Code
written for LMDB 0.9 can keep its 0.9 `lmdb.h` (put its directory first
in the include path); it links the same way.

The shared library needs nothing else. The static one (`librlmdb.a`,
`rlmdb.lib` with MSVC) needs a few system libraries:

| Platform | Add |
|---|---|
| Linux (glibc) | `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc` |
| macOS | `-liconv` |
| Windows (MinGW-w64) | `-lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp` |
| Windows (MSVC) | `legacy_stdio_definitions.lib kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib`, compile with `/MD` |

`pkg-config --static --libs rlmdb` prints them for your platform, the
BSDs included.

## Running existing programs on rlmdb

A program linked against a shared C LMDB runs on rlmdb if the loader finds a stub
first. Use the 0.9 stub for programs built against LMDB 0.9, the 1.0
stub for LMDB 1.0:

```sh
LD_LIBRARY_PATH=/usr/local/lib/rlmdb/lmdb-0.9 ./prog      # Linux, BSDs
DYLD_LIBRARY_PATH=/usr/local/lib/rlmdb/lmdb-0.9 ./prog    # macOS
```

On Windows, copy the stub next to the program as `lmdb.dll`. On macOS,
`DYLD_LIBRARY_PATH` does not reach system binaries, or programs started
from a shell script.

A program can tell that it runs on a stub from `mdb_version()`: the stubs
report 0.9.85 and 1.0.85, a patch level no C release has.

## Data formats

LMDB 0.9 and 1.0 use different file formats: C LMDB 0.9 cannot open 1.0
files and the other way round. rlmdb handles both. An existing file
always opens in its own format. A new environment is 0.9 by default, so
that every C LMDB 0.9 can open it; the 1.0 stub creates 1.0 files.

1.0 environments allow longer keys and page sizes from 256 bytes to
64 KiB (`mdb_env_set_pagesize`).

To create a 1.0 environment, ask for it before opening:

```c
mdb_env_create(&env);
rlmdb_env_set_format(env, RLMDB_FORMAT_10);
mdb_env_open(env, "./db", 0, 0644);
```

or for every new environment of the process:

```c
rlmdb_set_default_format(RLMDB_FORMAT_10);
```

### The `RLMDB_DEFAULT_FORMAT` variable

For programs you cannot change, the environment variable
`RLMDB_DEFAULT_FORMAT` sets the format of new environments:

```sh
RLMDB_DEFAULT_FORMAT=1.0 ./prog
```

It takes `0.9` or `1.0`; other values are ignored. It is experimental
and may go away. Since existing files keep their format, you only need it
when the environment is created.

The program's own choice always wins over the variable. To make sure the
variable never changes what your program creates, call this at start-up:

```c
rlmdb_set_default_format(RLMDB_FORMAT_09);
```

## Opt-in features

A feature is extra behaviour of rlmdb that C LMDB does not have. It is
off unless you turn it on. Turning a feature on never breaks
compatibility: C programs can still open, read and write the
environment.

This release has one feature, `test-feature`. It does nothing. It is
there so that you can try the functions below. It will be removed when
the first real feature is added.

Turn a feature on before you open the environment:

```c
mdb_env_create(&env);
rlmdb_env_enable_feature(env, "test-feature");
mdb_env_open(env, "./db", 0, 0644);
```

If you cannot change the program, use the `RLMDB_FEATURES` variable:

```sh
RLMDB_FEATURES=test-feature ./prog
```

To turn a feature off, put a `-` before its name. Separate names with
commas or spaces:

```sh
RLMDB_FEATURES="a, -b" ./prog    # a on, b off
```

If a name appears twice, the last one counts. If you call
`rlmdb_env_enable_feature()` or `rlmdb_env_disable_feature()` in the
program, that call wins over the variable.

A name the library does not know is an error: the function, or
`mdb_env_open()` for a name in the variable, returns
`RLMDB_UNKNOWN_FEATURE`. A typo in the variable stops the program from
opening the environment, so you notice it.

Each feature has a default:

| Default | Meaning |
|---|---|
| `RLMDB_FEATURE_OFF` | Off unless you turn it on. |
| `RLMDB_FEATURE_ON` | On unless you turn it off. |
| `RLMDB_FEATURE_FIXED` | Always on. Turning it off returns `RLMDB_FEATURE_IS_FIXED`. |

`rlmdb_features()` lists the features the library knows.
`rlmdb_version_string()` lists them too, for logs: a `*` after a name
means on by default, a `!` means always on.

## Environment variables

The library reads these variables when it opens an environment:

| Variable | What it does |
|---|---|
| `RLMDB_IGNORE_ENV` | If set, and not empty, the library ignores the other two. |
| `RLMDB_FEATURES` | Turns features on or off (see "Opt-in features"). |
| `RLMDB_DEFAULT_FORMAT` | Sets the format of new environments (experimental, see above). |

A setting the program makes with a function always wins over a variable.

A program that should not read the variables at all can turn them off
before it opens the environment:

```c
rlmdb_env_read_environment(env, 0);
```

Programs that run setuid or setgid never read the variables. The rlmdb
tools read them like any C program. Rust programs read them only if they
ask for it.

## The rlmdb functions

`rlmdb.h` declares rlmdb's own functions. C LMDB does not have them, so
a program that uses them needs rlmdb. Include it after `lmdb.h` (either
version), or on its own. The comments in [`include/rlmdb.h`](include/rlmdb.h)
document each function.

| Function | What it does |
|---|---|
| `rlmdb_env_set_format(env, format)` | Sets the format of a new environment, before `mdb_env_open()`. |
| `rlmdb_env_get_format(env, &format)` | Gets the format of an environment. |
| `rlmdb_set_default_format(format)` | Sets the format of all new environments of the process. |
| `rlmdb_sniff_format(path, flags, &format)` | Tells the format of an environment on disk, without opening it. |
| `rlmdb_env_set_pagesize(env, size)` | `mdb_env_set_pagesize()` for programs using 0.9's `lmdb.h`. |
| `rlmdb_env_enable_feature(env, name)` | Turns a feature on, before `mdb_env_open()`. |
| `rlmdb_env_disable_feature(env, name)` | Turns a feature off, before `mdb_env_open()`. |
| `rlmdb_env_feature_enabled(env, name, &on)` | Tells whether a feature is on. |
| `rlmdb_features()` | Lists the features the library knows. |
| `rlmdb_feature_default(name, &state)` | Tells a feature's default: off, on or always on. |
| `rlmdb_env_read_environment(env, on)` | Turns reading the environment variables on or off, before `mdb_env_open()`. |
| `rlmdb_version()` | Version numbers, as a struct. |
| `rlmdb_version_string()` | The same as one line, for logs. |

rlmdb does not convert files between formats: asking for the other
format of an existing file makes `mdb_env_open()` fail.

A complete example:

```c
#include <stdio.h>
#include "rlmdb.h"	/* includes lmdb.h */

int main(void)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_val key = { 5, "hello" }, data = { 5, "world" };
	int rc;

	printf("%s\n", rlmdb_version_string());

	/* A 1.0 environment in ./db (the directory must exist). */
	if ((rc = mdb_env_create(&env)) ||
	    (rc = rlmdb_env_set_format(env, RLMDB_FORMAT_10)) ||
	    (rc = mdb_env_open(env, "./db", 0, 0644)))
		goto fail;

	/* From here on it is plain LMDB. */
	if ((rc = mdb_txn_begin(env, NULL, 0, &txn)) ||
	    (rc = mdb_dbi_open(txn, NULL, 0, &dbi)) ||
	    (rc = mdb_put(txn, dbi, &key, &data, 0)) ||
	    (rc = mdb_txn_commit(txn)))
		goto fail;
	mdb_env_close(env);
	return 0;
fail:
	fprintf(stderr, "%s\n", mdb_strerror(rc));
	return 1;
}
```

## Differences from C LMDB

Most programs will not notice any. The ones you might notice:

* Read transactions are not tied to a thread (as if `MDB_NOTLS` were
  always set).
* Opening the same environment twice in one process fails with `EBUSY`,
  read-only too, unless `MDB_NOLOCK` is used. C allows it, but it is
  unsafe there.
* Mistakes that crash C, like passing NULL handles, return `EINVAL`.
* `MDB_WRITEMAP` is accepted, but rlmdb does not write through the map.
* Most known C LMDB bugs are fixed rather than copied
  ([KNOWN_C_BUGS.md](../KNOWN_C_BUGS.md)); the files stay the same.

Load only one rlmdb library into a process, not `librlmdb` and a stub
together: each has its own process-wide state.

[DETAILS.md](DETAILS.md) has the complete list of differences, the
undefined behaviour that remains, and how the library is tested against
C LMDB.
