# rlmdb: LMDB in Rust

[![CI](https://github.com/wojas/rlmdb/actions/workflows/ci.yml/badge.svg)](https://github.com/wojas/rlmdb/actions/workflows/ci.yml)
[![Nightly](https://github.com/wojas/rlmdb/actions/workflows/nightly.yml/badge.svg)](https://github.com/wojas/rlmdb/actions/workflows/nightly.yml)

A Rust rewrite of [LMDB](http://www.lmdb.tech/) that shares data and lock files with C LMDB.
Ships a Rust library, a drop-in `liblmdb` for C programs, and the `mdb_*` tools.

## Goals

* **C interoperability**: same data and lock files as C 0.9 and C 1.0, both supported in one library.
* **No footguns**: safe Rust interface, prevent API misuse.
* **Untrusted files**: every read bounds-checked. Damaged files give errors, not crashes.
* **Performance on par with C**: reads are, writes ~13% slower.
* **Drop-in C library**: upstream programs and tools run unchanged.

## Using it

### C programs

Build the library, compile with `-I rlmdb-capi/include` and link with `-lrlmdb`
instead of `-llmdb`:

```sh
cargo build --release -p librlmdb   # target/release/librlmdb.{a,so,dylib}
# macOS
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   target/release/librlmdb.a
# Linux (glibc): the static library needs the Rust runtime's system libs
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   target/release/librlmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
# shared library, after scripts/install-libs.sh (and ldconfig on Linux)
cc -o mtest tests/lmdb-c/0.9/mtest.c $(pkg-config --cflags --libs rlmdb)
```

The included `lmdb.h` is LMDB 1.0's; a program written for 0.9 can use 0.9's
`lmdb.h` and links the same way. `rlmdb.h` adds rlmdb's own `rlmdb_*` functions.
New environments are 0.9 unless you ask for 1.0, per environment or with
`rlmdb_set_default_format(RLMDB_FORMAT_10)` for the whole process:

```c
#include <stdio.h>
#include "rlmdb.h"	/* includes lmdb.h */

int main(void)
{
	MDB_env *env;
	int rc;

	if ((rc = mdb_env_create(&env)) ||
	    (rc = rlmdb_env_set_format(env, RLMDB_FORMAT_10)) ||	/* new files only */
	    (rc = mdb_env_open(env, "./env10", 0, 0644))) {	/* the directory must exist */
		fprintf(stderr, "%s\n", mdb_strerror(rc));
		return 1;
	}
	mdb_env_close(env);
	return 0;
}
```

Programs built against C LMDB can run on the stubs without a rebuild.
`scripts/install-libs.sh` installs them in `$libdir/rlmdb/lmdb-0.9` and
`$libdir/rlmdb/lmdb-1.0`, off the default library path, so that they never
replace a C `liblmdb` by accident. Put that directory first in the loader's
path (`DYLD_LIBRARY_PATH` on macOS; on Windows, copy the stub as `lmdb.dll`
next to the program). The 1.0 stub creates new environments as 1.0.

```sh
LD_LIBRARY_PATH=/usr/local/lib/rlmdb/lmdb-0.9 ./prog   # prog linked against C LMDB 0.9
```

### The Rust library

```toml
[dependencies]
rlmdb = { git = "https://github.com/wojas/rlmdb" }
```

```rust
use rlmdb::{Environment, Transaction, WriteFlags};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all("env")?; // the directory must exist
    let env = Environment::builder().open("env")?;
    let db = env.open_db(None)?;

    let mut txn = env.begin_rw_txn()?;
    txn.put(db, b"alice", b"1", WriteFlags::empty())?;
    txn.commit()?;

    let txn = env.begin_ro_txn()?;
    let mut cursor = txn.cursor(db)?;
    while let Some((key, value)) = cursor.next()? {
        println!("{key:?} = {value:?}");
    }
    Ok(())
}
```

A new 1.0 environment: `Environment::builder().format(rlmdb::Format::V10).open("env10")?`.


## Testing

```sh
cargo test --workspace
scripts/linux-test.sh    # the same tests in a Linux (glibc) container
scripts/check-all.sh     # fmt, clippy, docs, host and Linux tests
```

The tests build C LMDB 0.9.37 and 1.0 from `tests/lmdb-c/` and check Rust against them.

## Documents

* [`docs/DETAILS.md`](docs/DETAILS.md): what is implemented, platforms, tests, and links to the design documents
* [`rlmdb-capi/README.md`](rlmdb-capi/README.md): using the C library ([details](rlmdb-capi/DETAILS.md))
* [`COMPATIBILITY.md`](COMPATIBILITY.md): compatibility promises and their tests
* [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md): C bugs rlmdb fixes instead of copying
* [`bench/RESULTS.md`](bench/RESULTS.md): benchmarks against C

## License

Copyright 2026 rlmdbs contibutors. This is a derivative work of the LMDB C
library, copyright 2011-2021 Howard Chu, Symas Corp. Both are distributed
under the [OpenLDAP Public License, Version 2.8](LICENSE); see
[COPYRIGHT](COPYRIGHT) for the notices. The `Cargo.toml` files declare the
same license (`OLDAP-2.8`).
