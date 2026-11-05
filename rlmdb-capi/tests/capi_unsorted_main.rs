//! `tests/c/capi_unsorted_main.c`: a nested transaction's abort and the
//! creation of a database in a main DB out of order (a comparator changed
//! within a write transaction), in three environments: an abort before the
//! commit, the same with a database created after it, and a database
//! created without a nested transaction.
//!
//! A fourth environment has a main DB in order of several pages, with
//! databases created in a scrambled order over two transactions: the files
//! must match there too.
//!
//! The program runs against C LMDB 0.9 and the Rust library (0.9
//! environments), and against C LMDB 1.0 and the Rust LMDB 1.0 stub (1.0
//! environments). For each format the traces (`T ...` lines) of C and Rust
//! must be identical, the data files they leave must be the same byte for
//! byte (in the fourth environment except the unused space of pages, see
//! [`first_difference`]), and every environment must pass the C page audit
//! (`cpeer check-freelist`, built against the library of its format). Before
//! rlmdb's abort kept the handles of the parent's databases
//! (`KNOWN_C_BUGS.md` row 19), the first failed to commit with
//! `MDB_BAD_DBI` and the second lost the parent's put and leaked its page;
//! before a new database's record went where C's `mdb_dbi_open` puts it,
//! the third differed from C's file.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

/// The environments the program creates in its scratch directory, and
/// whether their files must be the same byte for byte (`false`: except the
/// unused space of pages).
const VARIANTS: [(&str, bool); 4] = [
    ("plain", true),
    ("reuse", true),
    ("nochild", true),
    ("sorted", false),
];

/// `PAGEHDRSZ` and `PAGEBASE` of the format the programs built against
/// `lib` write (`DUAL_FORMAT.md` §1).
fn page_header(lib: Lib) -> (usize, usize) {
    if lib.is_v10() {
        (24, 24)
    } else {
        (16, 0)
    }
}

/// The first byte at which data files `a` and `b` (with pages of the
/// header `hdr` of [`page_header`]) differ; with `exact` false, the unused
/// space between `mp_lower` and `mp_upper` of branch and leaf pages does
/// not count: C leaves there what a reused page buffer held before
/// (`interop::pagecmp` skips it too, and the padding byte of a node of odd
/// size, which the program avoids). The page size is the free DB record's
/// `md_pad` in the first meta page.
fn first_difference(a: &[u8], b: &[u8], (hdr, base): (usize, usize), exact: bool) -> Option<usize> {
    let at = |p: &[u8], off: usize| usize::from(u16::from_ne_bytes([p[off], p[off + 1]]));
    let psize = u32::from_ne_bytes(a[hdr + 24..hdr + 28].try_into().unwrap()) as usize;
    let pages = a.chunks(psize).zip(b.chunks(psize));
    for (i, (pa, pb)) in pages.enumerate() {
        let mut unused = 0..0;
        if !exact && pa.len() == psize && at(pa, hdr - 6) & 0x03 != 0 {
            // P_BRANCH or P_LEAF
            unused = (base + at(pa, hdr - 4)).min(psize)..(base + at(pa, hdr - 2)).min(psize);
        }
        let differs = |j: &usize| pa.get(*j) != pb.get(*j) && !unused.contains(j);
        if let Some(j) = (0..pa.len().max(pb.len())).find(differs) {
            return Some(i * psize + j);
        }
    }
    None
}

fn run_unsorted_main(lib: Lib) -> (String, PathBuf) {
    let b = builder();
    let exe = b.test_program("capi_unsorted_main", lib);
    let scratch = b.scratch(&format!("unsorted-main-{}", lib.name()));
    let out = Command::new(&exe).arg(&scratch).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_unsorted_main ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_unsorted_main against the {lib:?} library failed ({})",
        out.status
    );
    (stdout, scratch)
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

/// The C page audit of every environment in `scratch`, with the C peer
/// built against `oracle`.
fn check_environments(oracle: Lib, scratch: &Path) {
    let cpeer = builder().cpeer(oracle);
    for (v, _) in VARIANTS {
        run_ok(
            Command::new(&cpeer)
                .arg("check-freelist")
                .arg(scratch.join(v)),
        );
    }
}

/// Runs the program against `oracle` and `rust`, a C library and a Rust
/// library of the same format, and compares what they did and wrote.
fn matches_c(oracle: Lib, rust: Lib) {
    let (c_out, c_dir) = run_unsorted_main(oracle);
    let (r_out, r_dir) = run_unsorted_main(rust);
    let (tc, tr) = (trace(&c_out), trace(&r_out));
    assert!(tc.len() > 30, "{c_out}");
    // C keeps the parent's handle and writes the database's record.
    assert!(
        tc.contains(&"T plain: commit: 0") && tc.contains(&"T reuse: commit: 0"),
        "{c_out}"
    );
    for (i, (a, b)) in tc.iter().zip(&tr).enumerate() {
        assert_eq!(
            a, b,
            "trace line {i}: {oracle:?} (left) and {rust:?} (right) differ"
        );
    }
    assert_eq!(tc.len(), tr.len(), "trace lengths differ");
    for (v, exact) in VARIANTS {
        let c_file = std::fs::read(c_dir.join(v).join("data.mdb")).unwrap();
        let r_file = std::fs::read(r_dir.join(v).join("data.mdb")).unwrap();
        assert_eq!(c_file.len(), r_file.len(), "{v}: file lengths differ");
        if let Some(at) = first_difference(&c_file, &r_file, page_header(oracle), exact) {
            panic!("{v}: the data files of {oracle:?} and {rust:?} differ at byte {at}");
        }
    }
    check_environments(oracle, &c_dir);
    check_environments(oracle, &r_dir);
}

#[test]
fn unsorted_main_matches_c_0_9() {
    matches_c(Lib::C, Lib::Rust);
}

#[test]
fn unsorted_main_matches_c_1_0() {
    matches_c(Lib::C10, Lib::Rust10);
}
