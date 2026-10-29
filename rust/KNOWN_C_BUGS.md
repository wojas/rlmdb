# Known bugs and quirks in C LMDB 0.9.36 and what the Rust implementation does

Policy (see DESIGN.md): plain bugs are not reimplemented, but every deviation
must keep C and Rust processes interoperable on the same environment. Each
entry says whether it touches on-disk or lock-file state.

| # | Where in C | Behaviour | Rust behaviour | Interop impact |
|---|---|---|---|---|
| 1 | `mdb_load.c`, `flags = 0;` added for ITS#8662 | The first database's flags from the dump header (e.g. `dupsort=1`) are dropped. | `lmdb-tools` `mdb_load` keeps the header flags. | None: a tool bug; the file produced is simply what the user asked for. |
| 2 | `mdb_stat.c` | `mdb_stat -r` without `-a`/`-s`/`-f` exits 1 on success. | Rust `mdb_stat` exits 0. | None. |
| 3 | `mdb.c` key parsing in callers (mtest, tools) | Keys in the map are not NUL-terminated; reading past the last mapped page faults (SIGBUS). | Library never reads past a slice; callers get bounds-checked slices. | None. |
