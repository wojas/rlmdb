# rlmdb: LMDB in Rust

[![CI](https://github.com/wojas/rlmdb/actions/workflows/ci.yml/badge.svg)](https://github.com/wojas/rlmdb/actions/workflows/ci.yml)
[![Nightly](https://github.com/wojas/rlmdb/actions/workflows/nightly.yml/badge.svg)](https://github.com/wojas/rlmdb/actions/workflows/nightly.yml)

A Rust rewrite of [LMDB](http://www.lmdb.tech/) that shares data and lock files with C LMDB.
Ships a Rust library, a drop-in `liblmdb` for C programs and the `mdb_*` tools.

## Goals

* **C interoperability**: same data and lock files as C 0.9.36 and C 1.0; C and Rust processes on one environment at once.
* **No footguns**: safe Rust outside the OS layer; the types reject the classic LMDB misuse.
* **Untrusted files**: every read bounds-checked; damaged files give errors, not crashes.
* **Performance on par with C**: C's time per operation for reads and writes, without giving up safety.
* **0.9 and 1.0 formats**: both in one library, chosen per environment; new environments are 0.9 unless 1.0 is requested.
* **Compatible improvements**: features C lacks that leave the files usable by C processes.
* **Drop-in C library**: `liblmdb.a`/`.so`/`.dylib` with the unmodified `lmdb.h`; upstream programs and tools run unchanged.

Status of each goal, with the evidence: [`docs/DETAILS.md`](docs/DETAILS.md#goals-and-status).

## Performance

Linux x86-64, 4 KiB pages, environments in RAM ([setup and per-workload values](docs/DETAILS.md#performance-per-workload)).

| workloads | C -O2 (default) | C -O3 -flto | macOS |
|---|---|---|---|
| reads (7) | 6 % faster (0.94x) | 3 % faster (0.97x) | 15 % faster (0.85x) |
| writes (7, no appends) | 13 % slower (1.13x) | 22 % slower (1.22x) | 8 % faster (0.92x) |
| Lightning Stream scan | 8-19 % faster (0.81-0.92x) | same (1.00x) | 19 % faster (0.81x) |
| Lightning Stream update | 9 % slower (1.09x) | 13 % slower (1.13x) | 17 % faster (0.83x) |

* Ratio: Rust's time per operation divided by C's.
* Excluded: sequential appends (1.49x), `DUPSORT` (1.17-1.67x), `fsync`-bound commits (equal on a real disk).
* Gaps that matter: one-put transactions 1.32x, random deletes 1.23x, random puts and overwrites 1.1x ([`bench/RESULTS.md`](bench/RESULTS.md)).

## Rust

```toml
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

## C

```sh
cargo build -p rlmdb-capi --release   # target/release/liblmdb.{a,so,dylib}
```

```sh
# macOS
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   target/release/liblmdb.a
# Linux (glibc): the static library needs the Rust runtime's system libs
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   target/release/liblmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
# shared library, either platform
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   -L target/release -llmdb
```

```c
#include <stdio.h>
#include "rlmdb.h"	/* includes lmdb.h */

int main(void)
{
	MDB_env *env;
	int rc;

	if ((rc = mdb_env_create(&env)) ||
	    (rc = rlmdb_env_set_format(env, RLMDB_FORMAT_10)) ||	/* new files only */
	    (rc = mdb_env_open(env, "./env10", 0, 0644))) {
		fprintf(stderr, "%s\n", mdb_strerror(rc));
		return 1;
	}
	mdb_env_close(env);
	return 0;
}
```

Unmodified programs: `RLMDB_DEFAULT_FORMAT=1.0 ./mtest` creates new environments as 1.0 (experimental).

## Testing

```sh
cargo test --workspace   # host: macOS or Linux
scripts/linux-test.sh    # the same tests in a Linux (glibc) container
scripts/check-all.sh     # fmt, clippy, docs, host and Linux tests
```

The tests build C LMDB 0.9.36 and 1.0 from `tests/lmdb-c/` and check Rust against them.

## Documents

* [`docs/DETAILS.md`](docs/DETAILS.md): goals, crates, tests, layout, scope
* [`COMPATIBILITY.md`](COMPATIBILITY.md): compatibility promises and their tests
* [`DUAL_FORMAT.md`](DUAL_FORMAT.md): the 0.9 and 1.0 formats
* [`DESIGN.md`](DESIGN.md): file format and architecture
* [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md): C bugs rlmdb fixes instead of copying
* [`bench/RESULTS.md`](bench/RESULTS.md): benchmarks against C
* [`AGENTS.md`](AGENTS.md): rules for every change
* [`rlmdb-capi/README.md`](rlmdb-capi/README.md): the C library
* [`rlmdb-tools/README.md`](rlmdb-tools/README.md): the tools
* [`interop/README.md`](interop/README.md): the cross-process tests
* [`bench/README.md`](bench/README.md): the benchmark

## License

Copyright 2026 Konrad Wojas. Derivative work of the LMDB C library, copyright 2011-2021 Howard Chu, Symas Corp.
Both under the [OpenLDAP Public License, Version 2.8](LICENSE); notices in [COPYRIGHT](COPYRIGHT).
The `Cargo.toml` files declare the same license (`OLDAP-2.8`).
