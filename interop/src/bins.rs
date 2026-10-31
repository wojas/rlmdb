//! Paths of the C programs that `build.rs` compiles into `$OUT_DIR/bin`.
//!
//! They are plain C executables linked against `liblmdb` built from
//! `../tests/lmdb-c/0.9` with the upstream Makefile's flags, and are run as
//! subprocesses.
//!
//! The second oracle, LMDB 1.0, is built the same way from the vendored 1.0
//! sources in `../tests/lmdb-c/1.0` (the pinned `mdb.RE/1.0` commit, see
//! `README.md`): the functions ending in `10` ([`mdb_stat10`], [`mtest10`],
//! [`cpeer10`] …). Its executables are named after the 0.9 ones with `10`
//! appended, after an underscore when the name ends in a digit
//! (`mdb_stat10`, `mtest10`, `mtest2_10`).
//!
//! One more C program is linked at test time: [`cpeer_capi`], the C peer
//! linked against the *Rust* `liblmdb.a` of `rlmdb-capi`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::{Error, Result};

/// The C peer (`c/cpeer*.c`), implementing the command protocol of `README.md`.
pub fn cpeer() -> &'static Path {
    Path::new(env!("INTEROP_CPEER"))
}

/// `c/layout_dump.c`: prints sizes and offsets of LMDB's internal structs.
pub fn layout_dump() -> &'static Path {
    Path::new(env!("INTEROP_LAYOUT_DUMP"))
}

/// The upstream `mdb_stat` tool.
pub fn mdb_stat() -> &'static Path {
    Path::new(env!("INTEROP_MDB_STAT"))
}

/// The upstream `mdb_dump` tool.
pub fn mdb_dump() -> &'static Path {
    Path::new(env!("INTEROP_MDB_DUMP"))
}

/// The upstream `mdb_load` tool.
pub fn mdb_load() -> &'static Path {
    Path::new(env!("INTEROP_MDB_LOAD"))
}

/// The upstream `mdb_copy` tool.
pub fn mdb_copy() -> &'static Path {
    Path::new(env!("INTEROP_MDB_COPY"))
}

/// The upstream test programs `mtest`, `mtest2` … `mtest5`, by name.
///
/// (`mtest6` needs an `MDB_DEBUG` build and is not built, as upstream.)
pub fn mtest(name: &str) -> Option<&'static Path> {
    let p = match name {
        "mtest" => env!("INTEROP_MTEST"),
        "mtest2" => env!("INTEROP_MTEST2"),
        "mtest3" => env!("INTEROP_MTEST3"),
        "mtest4" => env!("INTEROP_MTEST4"),
        "mtest5" => env!("INTEROP_MTEST5"),
        _ => return None,
    };
    Some(Path::new(p))
}

/// Names of the upstream test programs that [`mtest`] knows.
pub const MTESTS: &[&str] = &["mtest", "mtest2", "mtest3", "mtest4", "mtest5"];

/// `rlmdb/tests/c/writeops.c` built against LMDB 0.9: the C side of the
/// write-path differential harness (`rlmdb/tests/common/differential.rs`).
pub fn writeops() -> &'static Path {
    Path::new(env!("INTEROP_WRITEOPS"))
}

/// [`writeops`] built against LMDB 1.0; its optional sixth argument sets
/// the page size of a new environment (`mdb_env_set_pagesize`).
pub fn writeops10() -> &'static Path {
    Path::new(env!("INTEROP_WRITEOPS10"))
}

/// [`writeops10`] against a 1.0 library built with `-DNDEBUG`, as C
/// release builds are: no `mdb_assert`s. For scripts that trip an assertion
/// on a state C's arithmetic handles (see `interop/build.rs`); every other
/// test takes the upstream flags of [`writeops10`].
pub fn writeops10_ndebug() -> &'static Path {
    Path::new(env!("INTEROP_WRITEOPS10_NDEBUG"))
}

/// The C peer built against the LMDB 1.0 header and library. Its
/// `check-freelist` decodes the 1.0 layout (`c/cpeer_pagewalk.c` picks the
/// layout from `MDB_VERSION_MAJOR`).
pub fn cpeer10() -> &'static Path {
    Path::new(env!("INTEROP_CPEER10"))
}

/// `c/layout_dump.c` built against the LMDB 1.0 `mdb.c`: the same lines as
/// [`layout_dump`] where the structures agree, plus the 1.0-only values
/// (`mp_txnid`, `MDB_ovpage`, the parts of `MDB_LOCK_FORMAT`, the maximum
/// key size per page size). It creates and removes small environments in a
/// fresh directory under `$TMPDIR`.
pub fn layout_dump10() -> &'static Path {
    Path::new(env!("INTEROP_LAYOUT_DUMP10"))
}

/// The upstream LMDB 1.0 `mdb_stat` tool.
pub fn mdb_stat10() -> &'static Path {
    Path::new(env!("INTEROP_MDB_STAT10"))
}

/// The upstream LMDB 1.0 `mdb_dump` tool.
pub fn mdb_dump10() -> &'static Path {
    Path::new(env!("INTEROP_MDB_DUMP10"))
}

/// The upstream LMDB 1.0 `mdb_load` tool.
pub fn mdb_load10() -> &'static Path {
    Path::new(env!("INTEROP_MDB_LOAD10"))
}

/// The upstream LMDB 1.0 `mdb_copy` tool.
pub fn mdb_copy10() -> &'static Path {
    Path::new(env!("INTEROP_MDB_COPY10"))
}

/// The upstream LMDB 1.0 `mdb_drop` tool (1.0 only; 0.9 has no `mdb_drop`).
pub fn mdb_drop10() -> &'static Path {
    Path::new(env!("INTEROP_MDB_DROP10"))
}

/// The upstream LMDB 1.0 test programs, by their upstream name (`mtest`,
/// `mtest2` … `mtest5`, `mtest_remap`, `mtest_enc`, `mtest_enc3`); see
/// [`MTESTS10`].
///
/// Not built: `mtest6` (needs `MDB_DEBUG`), `mtest_enc2` and `mtest_enc4`
/// (they load the libsodium crypto module).
pub fn mtest10(name: &str) -> Option<&'static Path> {
    let p = match name {
        "mtest" => env!("INTEROP_MTEST10"),
        "mtest2" => env!("INTEROP_MTEST2_10"),
        "mtest3" => env!("INTEROP_MTEST3_10"),
        "mtest4" => env!("INTEROP_MTEST4_10"),
        "mtest5" => env!("INTEROP_MTEST5_10"),
        "mtest_remap" => env!("INTEROP_MTEST_REMAP10"),
        "mtest_enc" => env!("INTEROP_MTEST_ENC10"),
        "mtest_enc3" => env!("INTEROP_MTEST_ENC3_10"),
        _ => return None,
    };
    Some(Path::new(p))
}

/// Upstream names of the LMDB 1.0 test programs that [`mtest10`] knows.
pub const MTESTS10: &[&str] = &[
    "mtest",
    "mtest2",
    "mtest3",
    "mtest4",
    "mtest5",
    "mtest_remap",
    "mtest_enc",
    "mtest_enc3",
];

/// The directory holding all of the above.
pub fn bin_dir() -> &'static Path {
    Path::new(env!("INTEROP_BIN_DIR"))
}

/// The C library sources (`tests/lmdb-c/0.9`).
pub fn lmdb_src() -> &'static Path {
    Path::new(env!("INTEROP_LMDB_SRC"))
}

/// The LMDB 1.0 sources the 1.0 programs were built from
/// (`tests/lmdb-c/1.0`, or `$LMDB10_DIR` at build time).
pub fn lmdb10_src() -> &'static Path {
    Path::new(env!("INTEROP_LMDB10_SRC"))
}

/// System libraries a C program needs next to the Rust `liblmdb.a` (as
/// `rlmdb-capi/README.md` lists them; on macOS the compiler driver adds
/// them).
fn rust_native_libs() -> &'static [&'static str] {
    if cfg!(target_os = "linux") {
        &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
    } else {
        &[]
    }
}

/// The Rust `liblmdb.a` built by cargo for `rlmdb-capi` (a dev-dependency of
/// this crate, so it is built before the tests): in `target/<profile>/deps`
/// next to the running test executable, or one level up.
pub fn rust_staticlib() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut dirs = Vec::new();
    if let Some(deps) = exe.parent() {
        dirs.push(deps.to_path_buf());
        if let Some(profile) = deps.parent() {
            dirs.push(profile.to_path_buf());
            dirs.push(profile.join("deps"));
        }
    }
    dirs.iter()
        .map(|d| d.join("liblmdb.a"))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "the Rust liblmdb.a (rlmdb-capi) is not next to {}: build it with \
                 `cargo test -p rlmdb-interop` (rlmdb-capi is a dev-dependency)",
                exe.display()
            ))
        })
}

/// Links `cpeer_capi` from the recipe `build.rs` wrote and the Rust
/// `liblmdb.a`; returns its path. Written under a temporary name and
/// renamed, so concurrent test processes never run a half-written file.
fn link_cpeer_capi() -> Result<PathBuf> {
    let recipe_path = env!("INTEROP_CPEER_CAPI_LINK");
    let recipe = std::fs::read_to_string(recipe_path)?;
    let (mut out, mut cc, mut args, mut objs) = (None, None, Vec::new(), Vec::new());
    for line in recipe.lines() {
        match line.split_once(' ') {
            Some(("out", v)) => out = Some(PathBuf::from(v)),
            Some(("cc", v)) => cc = Some(v.to_owned()),
            Some(("arg", v)) => args.push(v.to_owned()),
            Some(("obj", v)) => objs.push(v.to_owned()),
            _ => return Err(Error::new(format!("{recipe_path}: bad line {line:?}"))),
        }
    }
    let (Some(out), Some(cc)) = (out, cc) else {
        return Err(Error::new(format!("{recipe_path}: incomplete recipe")));
    };
    let lib = rust_staticlib()?;
    let tmp = out.with_extension(format!("tmp{}", std::process::id()));
    let mut cmd = Command::new(&cc);
    cmd.args(&args)
        .args(&objs)
        .arg(&lib)
        .arg("-pthread")
        .args(rust_native_libs())
        .arg("-o")
        .arg(&tmp);
    let res = cmd
        .output()
        .map_err(|e| Error::new(format!("running {cc}: {e}")))?;
    if !res.status.success() {
        return Err(Error::new(format!(
            "linking cpeer_capi failed: {cmd:?}\n{}{}",
            String::from_utf8_lossy(&res.stdout),
            String::from_utf8_lossy(&res.stderr)
        )));
    }
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// The C peer linked against the Rust `liblmdb.a` of `rlmdb-capi`
/// ([`crate::Peer::COnRust`]): the same C program as [`cpeer`], compiled
/// against `rlmdb-capi/include/lmdb.h`, with the library swapped.
///
/// Linked once per test process (the first call), so it always uses the
/// `liblmdb.a` the tests were built with.
pub fn cpeer_capi() -> Result<&'static Path> {
    static PATH: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();
    match PATH.get_or_init(|| link_cpeer_capi().map_err(|e| e.message().to_owned())) {
        Ok(p) => Ok(p),
        Err(e) => Err(Error::new(e.clone())),
    }
}
