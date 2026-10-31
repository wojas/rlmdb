# The C reference sources

C LMDB is rlmdb's specification and its test oracle: the tests build the C
library, its tools and its test programs and run them next to rlmdb, on
the same files (`COMPATIBILITY.md`). This directory holds those C sources,
copied from upstream LMDB, so that a build needs neither git nor network
access. Nothing here is linked into rlmdb's own libraries or tools.

| Directory | Upstream commit | What it is |
|-----------|-----------------|------------|
| `0.9/` | `a69ce886a57ff75a8e8b88b93780a2228e324ba5` on `mdb.RE/0.9` | LMDB 0.9.36 plus ITS#10615, ITS#10642 and ITS#10643 (`lmdb.h` says 0.9.36) |
| `1.0/` | `4865523ff0e1514d26a675d9cfd1d3a4b2b6c4c3` on `mdb.RE/1.0` | LMDB 1.0.2 plus the same fixes (the `LMDB_1.0.2` tag still has the DUPSORT page-count bug of ITS#10643; see `interop/README.md`) |

Both commits are in the upstream repository, <https://github.com/LMDB/lmdb>
(the files are in its `libraries/liblmdb/`), not in this one. Where the
documentation of this repository cites an upstream commit (`a69ce88`,
`4865523`, `e4cf950`, ...), it means that repository.

## Contents

* `0.9/`: the library (`mdb.c`, `midl.c`, `midl.h`, `lmdb.h`), the tools
  (`mdb_stat.c`, `mdb_dump.c`, `mdb_load.c`, `mdb_copy.c`) and the test
  programs `mtest.c` … `mtest6.c`.
* `1.0/`: the library with 1.0's crypto-module loader (`module.c`), the
  chacha8 cipher of the encryption tests (`chacha8.c`, `chacha8.h`), the
  tools (now with `mdb_drop.c`), `mtest.c` … `mtest5.c` and the `mtest_*`
  programs that need no external module (`mtest_remap.c`, `mtest_enc.c`,
  `mtest_enc3.c`). The list is the one `interop/build.rs` builds
  (`LMDB10_LIB_FILES` and `UPSTREAM10_PROGRAMS`).
* `LICENSE` and `COPYRIGHT` of upstream, in both directories.

Upstream's man pages, `Makefile`, documentation, `CHANGES`, `mplay.c` and
sample programs are not copied: the build scripts and test helpers compile
the sources themselves, with the flags of the upstream `Makefile`. For the
documentation of the C API and the tools, see upstream LMDB
(<http://www.lmdb.tech/doc/>, and the man pages `mdb_stat.1` … in
`libraries/liblmdb` of the upstream repository).

Who builds what:

* `interop/build.rs`: the library, tools, `mtest*` and the C peer, for both
  versions (`$LMDB10_DIR` points it at another 1.0 source directory);
* `bench/build.rs`: the 0.9 library for `cbench`;
* `rlmdb/tests/common/mod.rs`, `rlmdb-capi/tests/common/mod.rs`,
  `rlmdb-tools/tests/common/mod.rs`: the 0.9 library, tools and `mtest*`
  for the integration tests.

## Licence

The files are Copyright Howard Chu, Symas Corp. and are distributed under
the OpenLDAP Public License, Version 2.8 (`LICENSE`, `COPYRIGHT`), the
licence of rlmdb itself. `1.0/chacha8.c` is D. J. Bernstein's public-domain
ChaCha implementation, as in upstream.

## Patches

The files are always upstream's at the pinned commit with the patches of
`patches/0.9/` and `patches/1.0/` applied in name order; there are none at
the moment. `patches/README.md` describes the convention.

## Updating

1. Change the pinned commit (`LMDB09_COMMIT` or `LMDB10_COMMIT`) at the top
   of `scripts/update-c-sources.sh`, and its file list if the tests need
   another file.
2. Run `scripts/update-c-sources.sh`. It fetches the pinned commits from
   upstream (`git fetch --depth 1`) into a temporary repository, extracts
   the files, applies the patches and re-creates `0.9/` and `1.0/`,
   reporting what differed. A patch that no longer applies stops it; fix
   or drop the patch and run it again.
3. Update this file and the commit cited in the documentation
   (`DESIGN.md`, `COMPATIBILITY.md`, `DUAL_FORMAT.md`, `interop/README.md`),
   then run `scripts/check-all.sh`: a new C version is a new oracle, and
   tests that compare layouts or messages may need attention.

`scripts/update-c-sources.sh --check` only compares: it exits with status 1
when the tree is not upstream plus patches.
