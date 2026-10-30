//! The C API of rlmdb (`rlmdb-capi`) with LMDB 1.0 environments, against C
//! LMDB 1.0. The C program on rlmdb's side is `cpeer_capi`: the C peer
//! (`c/cpeer*.c`, an `lmdb.h` client) linked against the Rust `liblmdb.a`.
//! On C 1.0's side are `cpeer10` (the same peer built against C 1.0),
//! `mdb_load10`, `mdb_dump10`, `mdb_stat10` and the page audit (`cpeer10
//! check-freelist`).
//!
//! * rlmdb writes, C 1.0 reads: environments created through the C API
//!   with `RLMDB_DEFAULT_FORMAT=1.0` (the program asks for no format), and
//!   1.0 environments of 1 KiB and 64 KiB pages that `mdb_load10` created
//!   empty and the C API opens without a format request (it sniffs the
//!   file), filled and partly deleted through `mdb_put`, `mdb_del` and
//!   cursors, with inline values larger than 16 KiB on 64 KiB pages. C 1.0
//!   verifies every data set, its page audit passes, `mdb_dump10` and
//!   `mdb_stat10` agree with what the C API reads.
//! * C 1.0 writes, rlmdb reads: `cpeer10` fills, the C API verifies every
//!   data set (`mdb_get`, every cursor operation of the peer's `verify`);
//!   `mdb_load10` loads keys longer than LMDB 0.9's 511 bytes and the C API
//!   dumps them as `mdb_dump10` does.

use std::ffi::OsString;
use std::path::Path;

use interop::tools::{audit10, parse_dump, run_tool_with_env, MdbStat};
use interop::{bins, Output, TestEnv};
use rlmdb::{Environment, EnvironmentFlags, Format, Transaction};

/// Runs `bin` with `args` (and `stdin`), with `RLMDB_DEFAULT_FORMAT` set to
/// `default_format` or removed; returns its output, whatever its status.
fn run_any(
    bin: &Path,
    args: &[&str],
    default_format: Option<&str>,
    stdin: Option<&[u8]>,
) -> Output {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let envs = [("RLMDB_DEFAULT_FORMAT", default_format)];
    run_tool_with_env(bin, &args, &envs, stdin).expect("run a program")
}

/// As [`run_any`]; the program must succeed.
fn run(bin: &Path, args: &[&str], default_format: Option<&str>, stdin: Option<&[u8]>) -> Output {
    let out = run_any(bin, args, default_format, stdin);
    assert!(out.success(), "{out}");
    out
}

fn path(env: &TestEnv) -> &str {
    env.path().to_str().expect("a UTF-8 temp path")
}

fn cpeer_capi() -> &'static Path {
    bins::cpeer_capi().expect("link cpeer_capi")
}

/// The data sets of a 1.0 environment with `psize`-byte pages, as peer
/// arguments `<db> <count> <seed> [options]`, and the `delete --every`
/// applied to some: plain values with overflow values of two pages and
/// more, DUPSORT and DUPFIXED keys with duplicates in sub-pages and in
/// sub-DBs, integer keys, and on 64 KiB pages inline values of 20000
/// bytes (above LMDB 0.9's 16 KiB half page).
fn data_sets(psize: usize) -> Vec<(Vec<String>, Option<u32>)> {
    let dups = (psize / 16).max(40).to_string();
    let big = (2 * psize + 100).to_string();
    let set = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let mut sets = vec![
        (set(&["plain", "200", "1", "--big", &big]), Some(2)),
        (
            set(&["dups", "100", "2", "--dupsort", "--dups", &dups]),
            Some(3),
        ),
        (
            set(&["fixed", "100", "3", "--dupfixed", "--dups", &dups]),
            None,
        ),
        (set(&["ints", "100", "4", "--intkey"]), Some(5)),
    ];
    if psize >= 65536 {
        sets.push((set(&["inline", "60", "5", "--valsize", "20000"]), Some(4)));
    }
    sets
}

/// Fills `env` with [`data_sets`] through `bin`, then deletes from some.
fn fill(bin: &Path, env: &TestEnv, psize: usize) {
    let sets = data_sets(psize);
    for (set, _) in &sets {
        let mut args = vec!["fill", path(env)];
        args.extend(set.iter().map(String::as_str));
        run(bin, &args, None, None);
    }
    for (set, every) in &sets {
        let Some(every) = every else { continue };
        let every = every.to_string();
        let mut args = vec!["delete", path(env)];
        args.extend(set.iter().map(String::as_str));
        args.extend(["--every", &every]);
        run(bin, &args, None, None);
    }
}

/// `verify` of every data set of [`fill`] through `bin`.
fn verify(bin: &Path, env: &TestEnv, psize: usize) {
    for (set, every) in data_sets(psize) {
        let mut args = vec!["verify".to_owned(), path(env).to_owned()];
        args.extend(set);
        if let Some(every) = every {
            args.extend(["--deleted-every".to_owned(), every.to_string()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(bin, &args, None, None);
        assert!(out.stdout.starts_with("verified "), "{out}");
    }
}

/// A new, empty 1.0 environment of `psize`-byte pages made by `mdb_load10`
/// from an empty dump (`db_pagesize=`).
fn empty_env10(psize: usize) -> TestEnv {
    let env = TestEnv::new().expect("temp env");
    let header = format!(
        "VERSION=3\nformat=bytevalue\nmapsize=268435456\ndb_pagesize={psize}\n\
         HEADER=END\nDATA=END\n"
    );
    run(
        bins::mdb_load10(),
        &[path(&env)],
        None,
        Some(header.as_bytes()),
    );
    env
}

/// The `key=value` fields of a peer's output line starting with `prefix`.
fn field(out: &Output, prefix: &str, name: &str) -> String {
    let line = out
        .stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} line: {out}"));
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        .to_owned()
}

/// C 1.0's `mdb_env_get_maxkeysize` for pages of `ps` bytes.
fn maxkey10(ps: usize) -> usize {
    (((ps - 24) / 2) & !1) - 2 - 56
}

/// The records of database `db` as `cpeer_capi dump` prints them (through
/// the C API), as bytes.
fn capi_records(env: &TestEnv, db: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let out = run(cpeer_capi(), &["dump", path(env), db], None, None);
    let unhex = |h: &str| -> Vec<u8> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).expect("hex"))
            .collect()
    };
    out.stdout
        .lines()
        .skip(1) // db=<name>
        .map(|l| {
            let (k, v) = l.split_once('=').expect("key=value");
            (unhex(k), unhex(v))
        })
        .collect()
}

/// `mdb_dump10 -s db` of `env`, parsed.
fn c10_records(env: &TestEnv, db: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let out = run(bins::mdb_dump10(), &["-s", db, path(env)], None, None);
    let mut dbs = parse_dump(&out.stdout).expect("a dump");
    assert_eq!(dbs.len(), 1);
    dbs.remove(0).records
}

/// What C 1.0 sees of `env` after rlmdb's C API wrote it: the data sets,
/// the page audit, and per database the records the C API reads
/// (`cpeer_capi dump`) and `mdb_stat10`'s statistics, which also hold for
/// what rlmdb reads (`mdb_stat10` prints 1.0's per-database page size).
fn c10_checks(env: &TestEnv, psize: usize) {
    verify(bins::cpeer10(), env, psize);
    if let Err(e) = audit10(env.path()) {
        panic!("page size {psize}: {e}");
    }
    let stat10 = run(bins::mdb_stat10(), &["-a", path(env)], None, None);
    let stat10 = MdbStat::parse(&stat10.stdout).expect("mdb_stat10 output");
    // Read-only and without the lock file: no other process writes now.
    let renv = Environment::builder()
        .max_dbs(8)
        .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
        .open(env.path())
        .expect("open read-only");
    assert_eq!((renv.format(), renv.page_size()), (Format::V10, psize));
    let txn = renv.begin_ro_txn().unwrap();
    for (set, _) in data_sets(psize) {
        let db = set[0].as_str();
        assert_eq!(
            capi_records(env, db),
            c10_records(env, db),
            "page size {psize}, {db}"
        );
        let c = stat10.db(Some(db)).expect("mdb_stat10 lists the DB");
        let r = txn.stat(txn.open_db(Some(db)).unwrap()).unwrap();
        assert_eq!(
            (
                c.page_size,
                c.depth,
                c.branch_pages,
                c.leaf_pages,
                c.overflow_pages,
                c.entries
            ),
            (
                Some(psize as u32),
                r.depth,
                r.branch_pages as u64,
                r.leaf_pages as u64,
                r.overflow_pages as u64,
                r.entries as u64
            ),
            "page size {psize}, {db}"
        );
    }
}

/// A new environment created through the C API by a program that asks
/// for no format, with the experimental `RLMDB_DEFAULT_FORMAT=1.0`, at the
/// OS page size; then filled through the C API (variable unset: the file
/// decides) and read by C 1.0.
#[test]
fn capi_creates_1_0_with_the_default_format_variable() {
    let env = TestEnv::new().expect("temp env");
    let out = run(
        cpeer_capi(),
        &["create", path(&env), "268435456", "16"],
        Some("1.0"),
        None,
    );
    let psize: usize = field(&out, "created", "psize").parse().unwrap();
    assert_eq!(
        rlmdb::sniff_format(env.path(), EnvironmentFlags::empty()).unwrap(),
        Some(Format::V10)
    );
    // mdb_env_get_maxkeysize of the open 1.0 environment.
    assert_eq!(
        field(&out, "created", "maxkeysize"),
        maxkey10(psize).to_string()
    );
    fill(cpeer_capi(), &env, psize);
    c10_checks(&env, psize);
}

/// Without the variable a new environment through the C API is LMDB 0.9,
/// which C 1.0 refuses.
#[test]
fn capi_creates_0_9_by_default() {
    let env = TestEnv::new().expect("temp env");
    let out = run(
        cpeer_capi(),
        &["create", path(&env), "268435456", "16"],
        None,
        None,
    );
    assert_eq!(field(&out, "created", "maxkeysize"), "511");
    assert_eq!(
        rlmdb::sniff_format(env.path(), EnvironmentFlags::empty()).unwrap(),
        Some(Format::V09)
    );
    let out = run_any(bins::mdb_stat10(), &[path(&env)], None, None);
    assert!(!out.success(), "{out}");
}

/// 1.0 files of 1 KiB and 64 KiB pages (made by C 1.0) opened through the
/// C API without a format request, even with `RLMDB_DEFAULT_FORMAT=0.9`
/// (the file decides), filled through it and read by C 1.0.
#[test]
fn capi_writes_c10_files_at_1k_and_64k_pages() {
    for psize in [1024, 65536] {
        let env = empty_env10(psize);
        let out = run(cpeer_capi(), &["stat", path(&env)], Some("0.9"), None);
        assert_eq!(field(&out, "psize", "psize"), psize.to_string());
        assert_eq!(
            field(&out, "maxkeysize", "maxkeysize"),
            maxkey10(psize).to_string()
        );
        fill(cpeer_capi(), &env, psize);
        c10_checks(&env, psize);
    }
}

/// C 1.0 writes (`cpeer10` creates at the OS page size, fills, deletes);
/// the C API reads every data set as the peer's `verify` checks it, and
/// the pages C 1.0 left pass its own audit after rlmdb's read-only use.
#[test]
fn c10_writes_capi_reads() {
    let env = TestEnv::new().expect("temp env");
    let out = run(
        bins::cpeer10(),
        &["create", path(&env), "268435456", "16"],
        None,
        None,
    );
    let psize: usize = field(&out, "created", "psize").parse().unwrap();
    fill(bins::cpeer10(), &env, psize);
    verify(cpeer_capi(), &env, psize);
    for (set, _) in data_sets(psize) {
        let db = set[0].as_str();
        assert_eq!(capi_records(&env, db), c10_records(&env, db), "{db}");
    }
    // The C API attached to C 1.0's version 2 lock file and left it and
    // the data file as C 1.0 expects them.
    verify(bins::cpeer10(), &env, psize);
    if let Err(e) = audit10(env.path()) {
        panic!("{e}");
    }
}

/// Keys longer than LMDB 0.9's 511 bytes, loaded by `mdb_load10` at 4 KiB
/// and 64 KiB pages, dumped through the C API as `mdb_dump10` dumps them.
#[test]
fn c10_long_keys_read_through_capi() {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    for psize in [4096, 65536] {
        let max = maxkey10(psize);
        let mut input = format!(
            "VERSION=3\nformat=bytevalue\ndatabase=long\ntype=btree\nmapsize=268435456\n\
             db_pagesize={psize}\nHEADER=END\n"
        );
        for i in 0..40usize {
            let mut k = format!("k{i:03}").into_bytes();
            k.resize(512 + (i * 97) % (max - 511), b'a' + (i % 26) as u8);
            let v = vec![b'v'; 1 + (i * 331) % (psize / 3)];
            input.push_str(&format!(" {}\n {}\n", hex(&k), hex(&v)));
        }
        input.push_str("DATA=END\n");
        let env = TestEnv::new().expect("temp env");
        run(
            bins::mdb_load10(),
            &[path(&env)],
            None,
            Some(input.as_bytes()),
        );
        let records = capi_records(&env, "long");
        assert_eq!(records.len(), 40);
        assert!(records.iter().all(|(k, _)| k.len() > 511));
        assert_eq!(records, c10_records(&env, "long"), "page size {psize}");
    }
}
