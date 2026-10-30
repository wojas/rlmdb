# Supporting the LMDB 0.9 and 1.0 formats in one rlmdb

Status: investigation, 2026-10-30. Implemented since: stage 1 (the `Layout`
descriptor), stage 2 (the 1.0 data file: `DESIGN.md`, "The LMDB 1.0 data
file"; `COMPATIBILITY.md`), and of stage 4 the version report and the
format selection of the Rust API (`EnvironmentBuilder::format`,
`sniff_format`).

Question: can rlmdb read and write both the LMDB 0.9 (`MDB_DATA_VERSION 1`)
and the LMDB 1.0 (`MDB_DATA_VERSION 3`) formats from one code base, picking
the format per environment at run time? Upstream says no for the C library
("Including such support would only bloat the library", `upgrading.doc` on
`mdb.RE/1.0`); lmdb-go v2 (PowerDNS/lmdb-go#42) gets there by linking both C
libraries under renamed symbols. For rlmdb the answer is yes, and it is a
smaller job than it sounds: the two formats share every structure except the
page header, the node data offset, the overflow reference and the lock-file
header. The engine (B-tree, cursors, free list, spill, copy) is the same.

Sources compared: `libraries/liblmdb/mdb.c` on this branch (0.9.36 plus
ITS#10615/10642/10643, line numbers below are from that file) and
`origin/mdb.RE/1.0` at 4865523 (1.0.2 plus the same ITS#10642/10643 fixes,
"mdb10.c" below).

## 1. What differs on disk

| Item | 0.9 (`MDB_DATA_VERSION 1`) | 1.0 (`MDB_DATA_VERSION 3`) |
|---|---|---|
| Page header `PAGEHDRSZ` | 16: `pgno, pad, flags, lower/upper` | 24: `pgno, txnid, pad, flags, lower/upper` (mdb10.c 1040) |
| `mp_lower`, `mp_upper`, `mp_ptrs[]` | offsets from the page start (`PAGEBASE 0`) | offsets from the end of the header (`PAGEBASE PAGEHDRSZ`, ITS#7713): `NUMKEYS = lower >> 1`, `NODEPTR = p + ptrs[i] + 24`, fresh `upper = psize - 24` (mdb10.c 1134-1137, 9272) |
| `MAX_PAGESIZE` | 32 KiB | 64 KiB; `mdb_env_set_pagesize()` |
| Meta page | `MDB_meta` (136 bytes) at offset 16 | same struct at offset 24 (`MDB_metabuf`, mdb10.c 1406) |
| `MDB_meta.mm_flags` | 0.9 env flags | may carry `MDB_ENCRYPT 0x2000`; checksum size also recorded in the meta page and checked at open (mdb10.c 4858, 5656-5664) |
| `MDB_db` | 48 bytes | identical |
| Node header `MDB_node` | 8 bytes | identical |
| Node data offset `NODEDATA` | `mn_data + ksize` (mdb.c 1007) | `mn_data + EVEN(ksize)` (mdb10.c 1280) |
| `LEAFSIZE` | `8 + ksize + dsize` | `8 + EVEN(ksize) + EVEN(dsize)` (mdb10.c 1271): different split points and page fill |
| `F_BIGDATA` node data | 8-byte `pgno` | 24-byte `MDB_ovpage {pgno, txnid, pages}` (mdb10.c 1169) |
| Overflow run | header on first page | same, plus `mp_txnid`; `OVPAGES` subtracts checksum space |
| Sub-page (`P_SUBP`) flags byte | `P_LEAF\|P_DIRTY\|P_SUBP = 0x52` (mdb.c 6920) | `P_LEAF\|P_SUBP = 0x42` (mdb10.c 8806); `P_DIRTY 0x10` no longer exists |
| Free DB records | key `txnid`, value `MDB_IDL` | identical (mdb10.c 2780-2860, `mdb_freelist_save`) |
| `P_LEAF2` pages | keys from offset 16 | keys from offset 24 (`LEAF2KEY`) |
| Max key size | 511, fixed | `nodemax - 56` where `nodemax = (((psize - 24 - sums) / 2) & -2) - 2` (mdb10.c 5669-5673; measured with `layout_dump10`): 1978 on 4 KiB pages, 8122 on 16 KiB; only known after open |
| Page size range | OS page size, up to 32 KiB | `mdb_env_set_pagesize()`: any power of two from 256 to 65536; 1.0's own `mtest` creates 1 KiB-page environments, so a 1.0 reader must not assume the OS page size |
| Named database keys in the main DB | the name (`strlen(name)` bytes) | the name and its terminating NUL (`strlen(name) + 1`, upstream e4cf950); found while implementing stage 2 |

Lock file (`lock.mdb`), `MDB_LOCK_VERSION` 1 vs 2:

| Item | 0.9 | 1.0 |
|---|---|---|
| `MDB_txbody` | `magic, format, rmutex/rmname, txnid, numreaders` (mdb.c 772) | `magic, format, txnid, numreaders, rmutex/mutexid/semid` (mdb10.c 915) |
| `mti_format` | `1 + (PIDLOCK << 16)` | `2 + MDB_lock_desc << 12`; `lock_desc` encodes cache line, `sizeof(pid_t)`, `sizeof(pthread_t)`, `sizeof(MDB_txbody)`, the lock type and `sizeof(pthread_mutex_t)` (mdb10.c 969-1017), so it is a per-platform constant that must be computed, or read from the `layout_dump` helper |
| macOS semaphores | names stored in the file (`mtb_rmname`, uninitialised padding, see AGENTS.md) | a 64-bit FNV hash `mtb_mutexid`; names derived from it (`mdb_pack85`, mdb10.c 5939) |
| Reader slot `MDB_rxbody` | `txnid, pid, tid` | identical; table offset differs because the header differs |
| Measured (`layout_dump10`, interop README) | | macOS arm64: `MDB_LOCK_FORMAT 0x28570002`, `mtb_mutexid` at 24, `sizeof(MDB_txbody)` 32, no writer-lock cache line, readers at 64. Linux arm64 glibc: `0x2cd82002`, `mtb_rmutex` at 24 (48 bytes), `sizeof(MDB_txbody)` 72, `mt2_wmutex` at 128, readers at 192. Linux x86-64 (40-byte mutex) by the formula: `0x2c2f6002`, not measured yet |

Nothing else in the file format changed: magic, meta semantics (two meta
pages, newer `mm_txnid` wins), `MDB_db`, node header, branch node pgno
packing, dup sub-DBs, `MDB_INTEGERKEY` layouts, free-list records. (Beyond
the layout, what C 1.0 writes also differs in a few decisions: see
`KNOWN_C_BUGS.md`, section "LMDB 1.0".)

## 2. What a 1.0 writer must do beyond the layout

These are invariants C 1.0 peers rely on, so they are not optional:

1. **`mp_txnid` on every clean page equals the txnid that wrote it.**
   `mdb_page_flush` sets it on write (mdb10.c 4198), the compacting copy
   writes 1 (11436-11517). C 1.0 writers decide whether a page is already
   theirs from this field (`IS_WRITABLE`, `IS_MUTABLE`, `IS_DIRTY_NW`, 16
   call sites) instead of from a dirty-list lookup. A too-large value makes a
   later C writer modify a shared page in place. rlmdb keeps its dirty list
   (`core/dirty.rs`) and simply stamps the header when flushing; the in-memory
   `workid` scheme of 1.0 (`mt_workid`, `MDB_PGTXNID_FLAGBITS`) is C's
   bookkeeping and does not reach the file.
2. **`MDB_ovpage.op_txnid`** is the writing txn's id (mdb10.c 9455).
3. **Node sizes round key and data to even lengths**, so splits, "fits on
   page" checks and `MDB_RESERVE` pointers move by up to a byte per item.
   For the "same bytes as C" test oracle this means every size computation
   in `cursor_put.rs` (`leaf_size`, `branch_size`), `split.rs`,
   `page_ops.rs::node_size` and `cursor_del.rs` takes the format into account.
4. **`mp_lower`/`mp_upper`/`mp_ptrs` are header-relative.** Every reader and
   writer of these fields changes meaning, not just size.
5. **Lock file v2** with the platform-exact `mti_format`, header layout and
   reader-table offset, and 1.0's semaphore naming on macOS.
6. **Refuse what we do not implement** with 1.0's codes: `MDB_ENCRYPT` in
   `mm_flags` or a non-zero checksum size in the meta page returns
   `MDB_ENV_ENCRYPTION` / `MDB_BAD_CHECKSUM` territory rather than reading
   garbage. (Encryption and checksums change the usable page space; they can
   be added later behind the same `Format` descriptor.)

Features of 1.0 that are API, not format, and can come later without
touching the layout work: `mdb_txn_prepare` / `mdb_env_rollback` (2PC),
`MDB_PREVSNAPSHOT` (pick the older meta, mdb10.c 4783), read-only child
transactions of a write transaction, `mdb_env_incr_dump*` (walks `mp_txnid`),
`mdb_cursor_is_db`, `mdb_txn_flags`, `MDB_REMAP_CHUNKS` and the reader page
cache (`MDB_RPAGE_CACHE`, in-memory only; the source of lmdb-go#45).

## 3. How far the 0.9 layout reaches into rlmdb today

`format/mod.rs::layout` holds the offsets as `const`s. Use counts outside
`format/`:

| constant | uses | files |
|---|---|---|
| `PAGE_HDR_SIZE` | 56 | `page.rs`, `split.rs`, `cursor_put.rs`, `page_ops.rs`, `meta.rs`, `lockfile.rs`, `txn_rw.rs`, `reader.rs`, `env.rs`, `cursor_del.rs`, `commit.rs` |
| `NODE_SIZE` | 27 | mostly `cursor_put.rs`, `split.rs`, `page_ops.rs` |
| `NODE_DATA` / `data_offset` | 12 | `node.rs` and the write path |
| `F_BIGDATA` sites | 30 | read and write paths, `copy.rs`, tools |
| `P_DIRTY` sites | 29 | in-memory only; the byte reaches disk only in sub-pages |

`layout::` is imported by 13 files. The `PageRef`/`PageMut`/`NodeRef`
accessors already funnel every header read through a handful of methods
(`num_keys`, `size_left`, `node_ptr`, `leaf2_key`, `data_offset`), which is
where the format has to enter.

## 4. Proposed design

### 4.1 One engine, a `Format` value per environment

Add `rlmdb::Format { V09, V10 }` (names after the C release, as lmdb-go
does) and a small `Layout` descriptor derived from it:

```rust
pub struct Layout {
    pub format: Format,
    pub data_version: u32,      // 1 | 3
    pub page_hdr: usize,        // 16 | 24
    pub ptr_base: usize,        // 0 | 24   (PAGEBASE)
    pub even_nodes: bool,       // NODEDATA / LEAFSIZE rounding
    pub ovpage_size: usize,     // 8 | 24
    pub subpage_flags: u16,     // 0x52 | 0x42
    pub max_pagesize: usize,    // 0x8000 | 0x10000
    pub lock: LockLayout,       // version, format word, header offsets
}
```

`PageRef`/`PageMut`/`NodeRef` carry a `&'static Layout` (two statics, one
per format), obtained from the environment when the page is resolved. The
accessors branch on it; `num_keys` becomes
`(lower - (hdr - ptr_base)) >> 1`, `node_ptr(i)` adds `ptr_base`, and so on.
Everything above `format/` passes the layout along with the page size, which
it already threads through (`Environment.page_size`, `node_max`).

Why a run-time value and not a generic parameter: the branch is on a field
that sits next to the page pointer and is perfectly predicted; making
`format` and `core` generic over a `Layout` trait would touch every struct
and signature in 22 k lines and double the compiled core for the same
result. Measure with `bench` after the change (AGENTS.md §6); if the
descriptor shows up, the generic version is a mechanical follow-up because
every access already goes through the accessors.

Per-environment values that become format-dependent: `node_max`,
`max_key_size` (511 for V09; computed for V10, only after open),
`MAX_PAGESIZE`, the `Meta` offset in the page, `lockfile` offsets and the
`mti_format` word, and `write_initial_pages`.

### 4.2 Detection and selection at open

Order in `Environment::open` (`core/env.rs` 259): registration, lock file,
data file, `read_header`. Insert a sniff of the data file before the lock
file is touched, exactly as lmdb-go's `SniffFormat` does and for the same
reason: C sets up the lock file before reading the header, so a wrong-format
open rewrites or re-initialises `lock.mdb` under a correct peer. The sniff
reads 32 bytes:

* magic `0xBEEFC0DE` at 16 and version at 20: V09 (version must be 1),
* magic at 24 and version at 28: V10 (version must be 3),
* version 2 at either place: written by lmdb-js prerelease builds, refuse
  with `MDB_VERSION_MISMATCH`,
* missing or empty file: new environment, created in the requested format,
* anything else: `MDB_INVALID`.

Then the existing `read_header` validates the chosen format in full (both
metas, page flags, sizes).

What C does without a sniff, measured with both oracles (interop
`oracle10.rs`): on a **closed** environment the wrong-version opener holds
the lock file exclusively, rewrites it in its own format (a 1.0 lock file's
`0x28570002` becomes 0.9's `0x10001`) and then fails with `MDB_INVALID`,
because the meta page sits 8 bytes off and the `P_META`/magic check fails
before the version word is read. Only while the other version holds the
environment open does the lock-file check give `MDB_VERSION_MISMATCH`. The
data file is never touched. rlmdb must therefore decide the format before
it initialises the lock file, and its own error for an unparseable header
stays `MDB_INVALID` as in C. The format of an existing environment always
wins; a format requested explicitly that differs from the file is an error
(lmdb-go's `ErrFormatConflict`; for us `MDB_VERSION_MISMATCH` is the
closest C code, or a new `rlmdb`-specific one). There is no in-place
conversion.

The lock file follows the data format: a 0.9 data file gets a v1 lock file
because C 0.9 peers check `mti_format`. An existing lock file of the other
version is `MDB_VERSION_MISMATCH` unless we are the exclusive opener, which
is what C does when it re-initialises the table.

Default for new environments: V09, so files stay readable by everything
that ships today. Resolution order, same as lmdb-go: per-environment setter,
then process default setter, then the environment variable
`RLMDB_DEFAULT_FORMAT` (`0.9` | `1.0`), then V09. The variable is honoured
everywhere, including the drop-in `liblmdb`, and is documented as
**experimental**: it may be removed or restricted in a later release, so
programs and deployments should prefer the setters. The reason for the
caveat: a stray variable in a service's environment silently creates 1.0
files. An unparseable value is ignored (V09), never an error.

A requested format that differs from the file's is `MDB_VERSION_MISMATCH`,
the code C returns when a file's format is not the one the library
handles; no rlmdb-specific error codes are added. `rlmdb_sniff_format` and
`rlmdb_env_get_format` let a program find out which format the file has.

### 4.3 Rust API

* `EnvironmentBuilder::format(Format)`.
* `Environment::format() -> Format`, `Environment::max_key_size()` already
  exists and becomes per-environment.
* `rlmdb::sniff_format(path, flags) -> Result<Option<Format>>`.
* `rlmdb::set_default_format(Option<Format>)`.
* `rlmdb::version_info() -> &'static VersionInfo` with `rlmdb` (crate
  version), `lmdb` (what `mdb_version` reports, 0.9.85), `v09` and `v10`
  (the C releases each format matches, 0.9.36 and 1.0.2 today), and
  `capabilities: &[&str]`; `Display` prints the single-line form of §4.4.
  `VersionInfo::has(&self, cap)` for tests.
* `Environment::copy_to(.., CopyFormat)`: a compacting copy that re-encodes
  pages in the other format is natural here because rlmdb owns both
  encoders; it walks the tree like `MDB_CP_COMPACT` and rebuilds nodes. This
  is the one thing lmdb-go cannot offer (it needs dump and load). Not in the
  first cut; wanted soon after, where it can replace or reimplement
  `github.com/wojas/lmdbconvert`. Advertised as capability `convert-copy`
  once it exists.

### 4.4 C API: `rlmdb_*` additions, `lmdb.h` untouched

`rlmdb-capi` keeps shipping the unmodified 0.9.36 `lmdb.h` and the `mdb_*`
symbols. New entry points go in a second header `include/rlmdb.h`:

```c
#define RLMDB_FORMAT_UNKNOWN 0
#define RLMDB_FORMAT_09      9
#define RLMDB_FORMAT_10     10

int  rlmdb_env_set_format(MDB_env *env, unsigned format);   /* before mdb_env_open */
int  rlmdb_env_get_format(MDB_env *env, unsigned *format);  /* requested, or actual once open */
int  rlmdb_sniff_format(const char *path, unsigned flags, unsigned *format); /* MDB_NOSUBDIR honoured */
int  rlmdb_set_default_format(unsigned format);             /* process-wide, 0 resets */
```

`rlmdb.h` also carries the 1.0 constants a program needs with a 1.0
environment: `MDB_PREVSNAPSHOT`, `MDB_ENCRYPT`, and the 1.0 error codes
(`MDB_PROBLEM .. MDB_ADDR_BUSY`, -30779 .. -30769), under their upstream
names and values so that a program compiled against upstream 1.0's `lmdb.h`
sees the same numbers. `mdb_strerror` learns those messages.

No rlmdb-specific error codes: a format conflict is `MDB_VERSION_MISMATCH`,
an unreadable header `MDB_INVALID`, an unsupported 1.0 feature in a file
the 1.0 code for it (`MDB_ENV_ENCRYPTION`, `MDB_BAD_CHECKSUM`).

Version reporting:

* `mdb_version()` reports **0.9.85** ("LMDB 0.9.85: (rlmdb)"), so a program
  can tell rlmdb from any real 0.9.x release while staying in the 0.9
  series it is a drop-in for. The shipped `lmdb.h` keeps `MDB_VERSION_PATCH
  36`; a run-time patch level above the header's is the normal LMDB
  situation and `mdb_version` is what programs are told to check.
  `rlmdb::consts::MDB_VERSION_PATCH` and `lmdb_version()` change to 85.
* A new struct answers the real questions:

```c
typedef struct rlmdb_version_info {
	unsigned	size;		/* sizeof(rlmdb_version_info) of the library; fields are only ever appended */
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;	/* the Rust crate version */
	int		lmdb_major, lmdb_minor, lmdb_patch;	/* what mdb_version() reports: 0, 9, 85 */
	int		v09_major, v09_minor, v09_patch;	/* C release the 0.9 format and API match: 0, 9, 36 */
	int		v10_major, v10_minor, v10_patch;	/* C release the 1.0 format and API match: 1, 0, 2 */
	const char *const *capabilities;		/* NULL-terminated, sorted, short tokens */
} rlmdb_version_info;

const rlmdb_version_info *rlmdb_version(void);		/* static, never freed */
const char *rlmdb_version_string(void);			/* static, one line */
int rlmdb_has_capability(const char *name);		/* 1 or 0 */
```

  The one-line string prints everything a bug report needs:

```
rlmdb 0.1.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36; format 1.0 = LMDB 1.0.2; caps: convert-copy format-0.9 format-1.0 split-freelist)
```

  Capabilities are short lower-case tokens, one per feature a program may
  want to test for, added as features land and never removed while the
  feature exists: `format-0.9`, `format-1.0`, `split-freelist` (when the
  free-list work from the `freelist-split` branch is merged),
  `convert-copy`, `prevsnapshot`, `txn-prepare`, `checksum`, `encrypt`,
  `default-format-env` (while `RLMDB_DEFAULT_FORMAT` is honoured, so its
  experimental status is testable). The Rust side is `rlmdb::version_info()`
  (§4.3); the C struct is filled from it once at first call.

Behaviour of existing functions:

* `rlmdb_env_get_format` tells the format of an environment; `mdb_version`
  does not change with it.
* `mdb_env_get_maxkeysize` returns 511 before open unless V10 was requested,
  and the environment's value after open (1.0 documents it as only valid
  after open).
* `mdb_env_open` accepts `MDB_PREVSNAPSHOT` only for V10.
* `mdb_env_set_flags` rejects `MDB_ENCRYPT` until encryption exists.

Tools (`rlmdb-tools`): `mdb_load`, `mdb_copy` and anything that creates a
file get a `-F 0.9|1.0` option; `mdb_stat` and `mdb_dump` print the format.
The upstream tools of each release stay the oracle for their own format.

### 4.5 Tests and the C oracle

The test strategy (AGENTS.md §8, "C is the oracle") carries over because
`interop` runs the C peers as separate processes. There is no symbol clash,
so no renaming trick is needed: `interop/build.rs` builds a second
`liblmdb` from a 1.0 source tree and a second set of upstream tools and
`mtest*`, and every differential and interop test is parameterised by
format. Needed:

* The 1.0 C sources are **not copied into the tree**. `interop/build.rs`
  extracts them from git at build time with
  `git show <ref>:libraries/liblmdb/<file>` into `$OUT_DIR/lmdb10/` for
  `mdb.c`, `midl.c`, `midl.h`, `lmdb.h`, the tools and `mtest*`. The ref is
  a pinned commit hash in a `const LMDB10_REF` next to a comment naming the
  tag it follows, overridable with `RLMDB_LMDB10_REF` for experiments, and
  `cargo:rerun-if-env-changed`. Pin at least 4865523 (`mdb.RE/1.0` after
  ITS#10642/10643): the `LMDB_1.0.2` tag still has the DUPSORT page-count
  bug, which would otherwise be baked into the oracle. The pinned commit
  must be fetched (`git fetch origin mdb.RE/1.0`); the build script fails
  with that instruction when `git show` cannot find it.
* Container builds: `linux-test.sh` bind-mounts only the checkout at
  `/work`. In a linked worktree `.git` is a file pointing at the main
  repository's `.git/worktrees/<name>`, which is outside the mount, so `git
  show` fails there. The script also mounts `git rev-parse --git-common-dir`
  read-only at its own absolute path (the `.git` file holds that absolute
  path, and the worktree's `commondir` and `gitdir` entries resolve through
  it). Nothing is written to it, so read-only is enough.
* `layout_dump` extended to print 1.0's `PAGEHDRSZ`, `sizeof(MDB_ovpage)`,
  `MDB_LOCK_FORMAT`, `offsetof` of the lock-file fields and
  `mdb_env_get_maxkeysize` for the host page sizes; the Rust `LockLayout`
  for V10 is checked against it on both macOS and Linux.
* The write-differential layout checks (`write_differential.rs`,
  `mtest.rs`) run per format; the page-audit and the cross-process
  scenarios in `interop/` run with C 0.9, C 1.0 and Rust peers in every
  combination that shares a format, plus a scenario that opens one of each
  format in one process.
* `robust_*` and the mutation tests get 1.0 fixtures (sniffing must not
  trust the version word blindly: a V09-shaped header claiming 3, and vice
  versa).
* `KNOWN_C_BUGS.md` gets a 1.0 column where behaviour differs.

## 5. Upstream 1.x bugs found by lmdb-go (label `upstream-bug`)

* **#45, `mdb_cursor_close` use-after-free** when a read-only cursor is
  closed after its transaction ended (1.0.0, 1.0.1, still in 1.0.2:
  `MDB_CURSOR_UNREF` reads `mc_txn->mt_env` before the "do not peek" guard).
  Comes from the reader page cache, not the format. rlmdb cannot have it:
  cursors borrow their transaction. It matters for the oracle: the C 1.0
  peer and C programs in tests must close cursors before aborting, or carry
  lmdb-go's `cursor-close-after-txn.patch`.
* **#44, double free of `mt_u.dirty_list`** when a nested write transaction
  fails to begin under memory pressure (0.9.34+, 1.0.x; ITS#10342 fallout).
  Not format related; rlmdb's nested transactions own their lists. Only
  reachable under `malloc` failure, so it does not affect the differential
  tests.
* **#47, 0.9.36 regressions.** (a) `MDB_USE_PWRITEV` build break, Linux
  only, irrelevant to rlmdb except when building the oracle with that
  option. (b) ITS#9388 page counts for DUPSORT: this branch's
  `libraries/liblmdb` already carries the follow-up fixes (ITS#10642,
  ITS#10643, same on both release branches), so the oracle is right; the
  open point in #47 remains: files written by 0.9.35 count sub-tree pages
  differently and nothing marks which convention a file uses, so after an
  upgrade the counters are mixed. rlmdb follows C here (it must keep the
  same `MDB_db` numbers) and should say so in `KNOWN_C_BUGS.md`. (c) The
  intentional change that empty `MDB_DUPSORT` values are now
  `MDB_BAD_VALSIZE` is in both 0.9.36 and 1.0.

## 6. Decisions taken (owner, 2026-10-30)

1. `mdb_version()` reports 0.9.85. A new `rlmdb_version()` returns a struct
   with the rlmdb version, the 0.9 and 1.0 C releases each format matches,
   and a list of short capability tokens; `rlmdb_version_string()` prints
   all of it on one line. (§4.4)
2. `RLMDB_DEFAULT_FORMAT` is honoured everywhere, marked experimental so it
   can be removed if it causes trouble. (§4.2)
3. No new error codes: a format conflict is `MDB_VERSION_MISMATCH`. (§4.2,
   §4.4)
4. No copy of the 1.0 sources in the tree: the build extracts them from a
   pinned commit of the existing `mdb.RE/1.0` branch with `git show`. (§4.5)
5. The format-converting copy is out of the first cut, wanted soon after;
   it may replace or reimplement `github.com/wojas/lmdbconvert`. (§4.3)

## 7. Effort, in order

1. `Layout` descriptor and threading it through `format/` and the write
   path; V09 only, zero behaviour change, all tests green, bench A/B. This is
   most of the churn and is safe to do first.
2. V10 data layout: header, `PAGEBASE`, even nodes, `MDB_ovpage`, sub-page
   flags, `mp_txnid` stamping, max key size, 64 KiB pages. Read side first
   (dump/stat of files made by C 1.0), then write with the differential
   tests against C 1.0. Done; besides the items above it needed 1.0's
   NUL-terminated database names, `mdb_rebalance` loosening the root, the
   sub-pages' `mp_txnid`, `op_pages` where 1.0 trusts it (`mdb_drop`, the
   compacting copy), and the `mp_pad` C's scratch page leaves in sub-pages
   (`KNOWN_C_BUGS.md`, section "LMDB 1.0").
3. Lock file v2 on Linux and macOS, including the semaphore naming.
4. Sniffing, selection, defaults (`RLMDB_DEFAULT_FORMAT`, experimental),
   `rlmdb.h`, `rlmdb_version()` and the capability list, `mdb_version`
   0.9.85, tools options. The version
   function can land first, on its own, since it is useful without any
   format work.
5. Interop matrix with both C versions (1.0 sources via `git show` of the
   pinned commit, container mount for the common git dir); 1.0 fixtures for
   the robustness tests; docs (`AGENTS.md` §1 and §9, `COMPATIBILITY.md`,
   `DESIGN.md`).
6. Soon after: the format-converting copy (`convert-copy`), replacing
   `lmdbconvert`.
7. Later: `MDB_PREVSNAPSHOT`, 2PC, checksums, encryption.
