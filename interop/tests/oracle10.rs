//! The LMDB 1.0 oracle: the C library, tools and `mtest*` programs that
//! `build.rs` builds from the pinned `mdb.RE/1.0` commit (see `README.md`).
//!
//! The tests check that the 1.0 build is what it claims to be (version, data
//! and lock formats), pin the 1.0 layout numbers the Rust implementation of
//! the 1.0 format will have to reproduce (page header, `MDB_ovpage`, lock-file
//! header, `MDB_LOCK_FORMAT`, maximum key size), and show how the two C
//! versions refuse each other's environments.

use std::ffi::OsString;
use std::path::Path;

use interop::tools::{c_layout10, run_tool_any_status, Layout};
use interop::{bins, Output, Peer, PeerProcess, TestEnv};

fn layout10() -> &'static Layout {
    c_layout10().expect("layout_dump10")
}

fn get(name: &str) -> u64 {
    layout10().get(name).expect(name)
}

/// Run a C program, whatever its exit status.
fn run(bin: &Path, args: &[&OsString], stdin: Option<&[u8]>) -> Output {
    let args: Vec<OsString> = args.iter().map(|a| (*a).clone()).collect();
    run_tool_any_status(bin, &args, None, stdin).expect("run C program")
}

/// One record, `key` = `val`, in `mdb_dump` format.
const DUMP: &[u8] = b"VERSION=3\nformat=bytevalue\nHEADER=END\n 6b6579\n 76616c\nDATA=END\n";

/// A new environment holding [`DUMP`], written by `mdb_load` (`load`: the
/// 0.9 or the 1.0 build).
fn loaded_env(load: &Path) -> TestEnv {
    let env = TestEnv::new().expect("temp env");
    let path = env.path().as_os_str().to_owned();
    run(load, &[&path], Some(DUMP)).ok().expect("mdb_load");
    env
}

/// Does `stat` (an `mdb_stat` build) fail on `env` with `error` in its
/// message?
fn assert_stat_fails(stat: &Path, env: &TestEnv, error: &str) {
    let path = env.path().as_os_str().to_owned();
    let out = run(stat, &[&path], None);
    assert!(!out.success(), "{out}");
    assert!(
        out.stderr.contains(error) || out.stdout.contains(error),
        "expected {error}: {out}"
    );
}

/// Does `dump` (an `mdb_dump` build) still read [`DUMP`]'s record from `env`?
fn assert_readable(dump: &Path, env: &TestEnv) {
    let path = env.path().as_os_str().to_owned();
    let out = run(dump, &[&path], None).ok().expect("mdb_dump");
    assert!(out.stdout.contains(" 6b6579\n 76616c\n"), "{out}");
}

#[test]
fn tools_report_their_version() {
    for tool in [
        bins::mdb_stat10(),
        bins::mdb_dump10(),
        bins::mdb_load10(),
        bins::mdb_copy10(),
        bins::mdb_drop10(),
    ] {
        let v = OsString::from("-V");
        let out = run(tool, &[&v], None).ok().expect("-V");
        assert!(out.stdout.starts_with("LMDB 1.0."), "{out}");
    }
    let v = OsString::from("-V");
    let out = run(bins::mdb_stat(), &[&v], None).ok().expect("-V");
    assert!(out.stdout.starts_with("LMDB 0.9."), "{out}");
}

#[test]
fn format_versions_and_page_header() {
    assert_eq!(get("MDB_MAGIC"), 0xBEEF_C0DE);
    assert_eq!(get("MDB_DATA_VERSION"), 3);
    assert_eq!(get("MDB_LOCK_VERSION"), 2);
    assert_eq!(get("MDB_DEVEL"), 0);
    assert_eq!(get("MAX_PAGESIZE"), 0x10000);
    // Computed per environment (see `max_key_size`), not fixed at 511.
    assert_eq!(get("MDB_MAXKEYSIZE"), 0);
    // pgno, txnid, pad, flags, lower/upper; node offsets count from the end
    // of the header (ITS#7713).
    assert_eq!(get("PAGEHDRSZ"), 24);
    assert_eq!(get("PAGEBASE"), 24);
    assert_eq!(get("sizeof(MDB_page)"), 24);
    for (field, offset, size) in [
        ("mp_p.p_pgno", 0, 8),
        ("mp_txnid", 8, 8),
        ("mp_pad", 16, 2),
        ("mp_flags", 18, 2),
        ("mp_pb.pb.pb_lower", 20, 2),
        ("mp_pb.pb.pb_upper", 22, 2),
        ("mp_pb.pb_pages", 20, 4),
    ] {
        assert_eq!(
            get(&format!("offsetof(MDB_page.{field})")),
            offset,
            "{field}"
        );
        assert_eq!(get(&format!("sizeof(MDB_page.{field})")), size, "{field}");
    }
    assert_eq!(get("offsetof(MDB_page.mp_ptrs)"), 24);
    // The data of an F_BIGDATA node: {pgno, txnid, pages}.
    assert_eq!(get("sizeof(MDB_ovpage)"), 24);
    assert_eq!(get("offsetof(MDB_ovpage.op_pgno)"), 0);
    assert_eq!(get("offsetof(MDB_ovpage.op_txnid)"), 8);
    assert_eq!(get("offsetof(MDB_ovpage.op_pages)"), 16);
    // The meta page holds the 0.9 MDB_meta, after the longer header.
    assert_eq!(get("sizeof(MDB_meta)"), 136);
    assert_eq!(get("offsetof(MDB_metabuf.mb_metabuf.mm_meta)"), 24);
    assert_eq!(get("sizeof(MDB_db)"), 48);
    assert_eq!(get("NODESIZE"), 8);
    assert_eq!(get("P_SUBP"), 0x40);
    assert_eq!(get("P_DIRTY_OVF"), 0x2000);
    assert_eq!(get("MDB_RPAGE_CACHE"), 1);
}

/// `log2(p2) % ((1 << n) - 1)` for a power of two, as mdb.c's `LOG2_MOD`.
fn log2_mod(p2: u64, n: u32) -> u64 {
    7 - 86 / (p2 % ((1 << n) - 1) + 11)
}

/// `MDB_LOCK_FORMAT` is the lock version in the low 12 bits plus
/// `MDB_lock_desc`, which mdb.c computes from the lock type and the sizes
/// of the lock-file items. Recomputing it from the sizes `layout_dump10`
/// prints pins the formula a Rust 1.0 lock file must follow.
#[test]
fn lock_format() {
    let l = layout10();
    let lock_type = match l.lock_mode.as_str() {
        "win32" => get("alignof(mdb_hash_t)") / 8 % 2,
        "posix_sem" => 4 + get("alignof(mdb_hash_t)") / 8 % 2,
        // System V semaphores (OpenBSD, NetBSD, DragonFly).
        "sysv_sem" => 8,
        "posix_mutex" => {
            10 + log2_mod(get("alignof(pthread_mutex_t)"), 5)
                + get("sizeof(pthread_mutex_t)") / 4 % 22 * 5
        }
        mode => panic!("unexpected lock mode {mode}"),
    };
    assert_eq!(get("MDB_LOCK_TYPE"), lock_type);
    let cacheline = get("CACHELINE");
    assert_eq!(cacheline, 64, "the CACHELINE term is 0 only for 64");
    let desc = 6 * (get("sizeof(pid_t)") / 4 % 3)
        + 18 * (get("sizeof(pthread_t)") / 4 % 5)
        + 90 * (get("sizeof(MDB_txbody)") / cacheline % 3)
        + 270 * (lock_type % 120)
        + (u64::from(get("sizeof(txnid_t)") == 8) << 15)
        + (u64::from(get("sizeof(MDB_reader)") > cacheline) << 16)
        + (u64::from(get("MDB_PIDLOCK") != 0) << 17);
    assert_eq!(get("MDB_lock_desc"), desc);
    let bits = get("MDB_LOCK_VERSION_BITS");
    assert_eq!(bits, 12);
    let format = get("MDB_LOCK_FORMAT");
    assert_eq!(format, get("MDB_LOCK_VERSION") + (desc << bits));
    assert!(format <= u64::from(u32::MAX));
    println!("lock_mode={} MDB_LOCK_FORMAT={format:#x}", l.lock_mode);
}

/// The lock-file header of 1.0: `mtb_txnid` and `mtb_numreaders` moved in
/// front of the reader lock, and with named semaphores (macOS) the names are
/// an 8-byte hash and there is no second cache line for the writer lock.
#[test]
fn lock_file_header() {
    let l = layout10();
    for (field, offset) in [
        ("mtb_magic", 0),
        ("mtb_format", 4),
        ("mtb_txnid", 8),
        ("mtb_numreaders", 16),
    ] {
        assert_eq!(get(&format!("offsetof(MDB_txbody.{field})")), offset);
        assert_eq!(
            get(&format!("offsetof(MDB_txninfo.mt1.mtb.{field})")),
            offset
        );
    }
    assert_eq!(get("sizeof(MDB_reader)"), 64);
    let readers = get("offsetof(MDB_txninfo.mti_readers)");
    match l.lock_mode.as_str() {
        // Named semaphores (macOS) or named mutexes (Windows).
        "posix_sem" | "win32" => {
            assert_eq!(get("offsetof(MDB_txbody.mtb_mutexid)"), 24);
            assert_eq!(get("sizeof(MDB_txbody.mtb_mutexid)"), 8);
            assert_eq!(get("sizeof(MDB_txbody)"), 32);
            assert!(layout10().get("offsetof(MDB_txninfo.mt2)").is_err());
            assert_eq!(readers, 64);
        }
        "posix_mutex" => {
            let size = get("sizeof(pthread_mutex_t)");
            let align = get("alignof(pthread_mutex_t)");
            let rmutex = 20u64.next_multiple_of(align);
            assert_eq!(get("offsetof(MDB_txbody.mtb_rmutex)"), rmutex);
            let txbody = (rmutex + size).next_multiple_of(align.max(8));
            assert_eq!(get("sizeof(MDB_txbody)"), txbody);
            let mt1 = txbody.next_multiple_of(64);
            assert_eq!(get("offsetof(MDB_txninfo.mt2.mt2_wmutex)"), mt1);
            assert_eq!(readers, mt1 + size.next_multiple_of(64));
            assert_eq!(get("MDB_ROBUST_SUPPORTED"), 1);
        }
        // System V semaphores (OpenBSD, NetBSD, DragonFly): the set's id
        // and the reader flag, two `int`s, follow the reader count; the
        // writer flag has the second cache line (`COMPATIBILITY.md`,
        // `format::lockfile::LOCK_V10`).
        "sysv_sem" => {
            assert_eq!(get("offsetof(MDB_txbody.mtb_semid)"), 20);
            assert_eq!(get("sizeof(MDB_txbody.mtb_semid)"), 4);
            assert_eq!(get("offsetof(MDB_txbody.mtb_rlocked)"), 24);
            assert_eq!(get("sizeof(MDB_txbody.mtb_rlocked)"), 4);
            assert_eq!(get("sizeof(MDB_txbody)"), 32);
            assert_eq!(get("offsetof(MDB_txninfo.mt1.mtb.mtb_semid)"), 20);
            assert_eq!(get("offsetof(MDB_txninfo.mt2.mt2_wlocked)"), 64);
            assert_eq!(readers, 128);
        }
        mode => panic!("unexpected lock mode {mode}"),
    }
    assert_eq!(get("offsetof(MDB_txninfo.mti_readers[1])"), readers + 64);
}

/// 1.0 computes the maximum key size from the page size at open:
/// `nodemax = (((psize - PAGEHDRSZ) / 2) & !1) - sizeof(indx_t)`, `maxkey =
/// nodemax - NODESIZE - sizeof(MDB_db)` (mdb10.c `mdb_env_open2`).
#[test]
fn max_key_size() {
    let os = get("os_page_size").min(get("MAX_PAGESIZE"));
    for (name, psize) in [("os_page_size", os), ("4096", 4096), ("16384", 16384)] {
        assert_eq!(get(&format!("me_psize({name})")), psize, "{name}");
        let nodemax = (((psize - get("PAGEHDRSZ")) / 2) & !1) - 2;
        assert_eq!(get(&format!("me_nodemax({name})")), nodemax, "{name}");
        assert_eq!(
            get(&format!("mdb_env_get_maxkeysize({name})")),
            nodemax - get("NODESIZE") - get("sizeof(MDB_db)"),
            "{name}"
        );
    }
    assert_eq!(get("mdb_env_get_maxkeysize(4096)"), 1978);
    assert_eq!(get("mdb_env_get_maxkeysize(16384)"), 8122);
}

/// The names `layout_dump` prints, in order.
fn names(bin: &Path) -> Vec<String> {
    let out = run(bin, &[], None).ok().expect("layout_dump");
    out.lines()
        .map(|l| l.split_once('=').expect("name=value").0.to_owned())
        .collect()
}

/// `layout_dump` and `layout_dump10` print the same names in the same order,
/// apart from what exists in one version only, so the two outputs can be
/// compared line by line.
#[test]
fn layout_dumps_line_up() {
    let n09 = names(bins::layout_dump());
    let n10 = names(bins::layout_dump10());
    let common09: Vec<&String> = n09.iter().filter(|n| n10.contains(n)).collect();
    let common10: Vec<&String> = n10.iter().filter(|n| n09.contains(n)).collect();
    assert_eq!(common09, common10);

    let mut only09: Vec<&str> = n09
        .iter()
        .filter(|n| !n10.contains(n))
        .map(String::as_str)
        .collect();
    only09.sort_unstable();
    let mut expected09 = vec!["P_DIRTY"];
    match layout10().lock_mode.as_str() {
        "posix_sem" | "win32" => expected09.extend([
            "offsetof(MDB_txbody.mtb_rmname)",
            "sizeof(MDB_txbody.mtb_rmname)",
            "offsetof(MDB_txninfo.mt1.mtb.mtb_rmname)",
            "sizeof(MDB_txninfo.mt1.mtb.mtb_rmname)",
            "offsetof(MDB_txninfo.mt2)",
            "sizeof(MDB_txninfo.mt2)",
            "offsetof(MDB_txninfo.mt2.mt2_wmname)",
            "sizeof(MDB_txninfo.mt2.mt2_wmname)",
        ]),
        // OpenBSD, NetBSD, DragonFly: 0.9 names its semaphores, 1.0 keeps
        // the id of a System V set (and has `mt2`, for the writer flag).
        "sysv_sem" => expected09.extend([
            "offsetof(MDB_txbody.mtb_rmname)",
            "sizeof(MDB_txbody.mtb_rmname)",
            "offsetof(MDB_txninfo.mt1.mtb.mtb_rmname)",
            "sizeof(MDB_txninfo.mt1.mtb.mtb_rmname)",
            "offsetof(MDB_txninfo.mt2.mt2_wmname)",
            "sizeof(MDB_txninfo.mt2.mt2_wmname)",
        ]),
        _ => {}
    }
    expected09.sort_unstable();
    assert_eq!(only09, expected09, "printed by the 0.9 layout_dump only");

    const ONLY_10: &[&str] = &[
        "MDB_LOCK_VERSION_BITS",
        "MDB_lock_desc",
        "MDB_LOCK_TYPE",
        "MDB_RPAGE_CACHE",
        "P_DIRTY_OVF",
        "(mdb_size_t)",
        "(mdb_hash_t)",
        "(MDB_page.mp_txnid)",
        "MDB_ovpage",
        "mtb_mutexid)",
        "mtb_semid)",
        "mtb_rlocked)",
        "mt2_wlocked)",
        "me_psize(",
        "me_nodemax(",
        "mdb_env_get_maxkeysize(",
    ];
    for name in n10.iter().filter(|n| !n09.contains(n)) {
        assert!(
            ONLY_10.iter().any(|p| name.contains(p)),
            "{name} is printed by layout_dump10 only: list it in ONLY_10"
        );
    }
}

/// A C 0.9 process and a C 1.0 process refuse each other's environments.
///
/// On a closed environment the opener is exclusive: it re-initialises the
/// lock file for its own version and then rejects the data file as
/// `MDB_INVALID`, because the meta page sits after a 16- or 24-byte page
/// header and the other version's header check fails before it reaches the
/// version word. The data file is not touched, and the right version still
/// reads it. While a process of the other version holds the environment
/// open, the opener is not exclusive and fails on the lock file's format
/// word with `MDB_VERSION_MISMATCH`.
#[test]
fn versions_refuse_each_other() {
    let wait = OsString::from("60");
    let until_stdin = OsString::from("--until-stdin");

    // A 1.0 environment, seen by 0.9.
    let env10 = loaded_env(bins::mdb_load10());
    assert_stat_fails(bins::mdb_stat(), &env10, "MDB_INVALID");
    assert_readable(bins::mdb_dump10(), &env10);
    let mut holder = PeerProcess::spawn_program(
        bins::cpeer10(),
        &[
            OsString::from("hold-env"),
            env10.path().as_os_str().to_owned(),
            wait.clone(),
            until_stdin.clone(),
        ],
    )
    .expect("start cpeer10 hold-env");
    holder.wait_ready().expect("cpeer10 hold-env READY");
    assert_stat_fails(bins::mdb_stat(), &env10, "MDB_VERSION_MISMATCH");
    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer10 hold-env");
    assert_readable(bins::mdb_dump10(), &env10);

    // A 0.9 environment, seen by 1.0.
    let env09 = loaded_env(bins::mdb_load());
    assert_stat_fails(bins::mdb_stat10(), &env09, "MDB_INVALID");
    assert_readable(bins::mdb_dump(), &env09);
    let mut holder = Peer::C
        .on("hold-env", &env09)
        .args([&wait, &until_stdin])
        .spawn()
        .expect("start cpeer hold-env");
    holder.wait_ready().expect("cpeer hold-env READY");
    assert_stat_fails(bins::mdb_stat10(), &env09, "MDB_VERSION_MISMATCH");
    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer hold-env");
    assert_readable(bins::mdb_dump(), &env09);
}

/// The 1.0 build of the C peer works on a 1.0 environment through the
/// public API (`create`, `fill`, `verify`, `stat`).
#[test]
fn cpeer10_fills_and_verifies() {
    let env = TestEnv::new().expect("temp env");
    let path = env.path().as_os_str().to_owned();
    let peer = |args: &[&str]| {
        let mut all: Vec<OsString> = vec![OsString::from(args[0]), path.clone()];
        all.extend(args[1..].iter().map(OsString::from));
        let all: Vec<&OsString> = all.iter().collect();
        run(bins::cpeer10(), &all, None)
            .ok()
            .expect("cpeer10 command")
    };
    let out = peer(&["create", "67108864", "4"]);
    assert!(out.stdout.contains("maxkeysize="), "{out}");
    peer(&["fill", "-", "200", "7"]);
    peer(&["verify", "-", "200", "7"]);
    let out = peer(&["stat"]);
    assert!(out.stdout.contains("entries=200"), "{out}");
    // It is a 1.0 environment: the 0.9 tools refuse it, the 1.0 ones read it.
    assert_stat_fails(bins::mdb_stat(), &env, "MDB_INVALID");
    let out = run(bins::mdb_stat10(), &[&path], None)
        .ok()
        .expect("mdb_stat10");
    assert!(out.stdout.contains("Entries: 200"), "{out}");
}

/// Every 1.0 `mtest*` program that is built runs to completion, and leaves
/// an environment that the 1.0 `mdb_stat` reads (the encrypted ones need
/// their key, which `mdb_stat` does not have). Not the `MDB_REMAP_CHUNKS`
/// programs (`mtest_remap`; `mtest_enc*`, encryption implies the flag) on
/// OpenBSD: for some of the sequences `rand()` gives them, C 1.0 fails its
/// own assertions there (`mdb_page_touch`, `mdb_page_dirty`), and
/// OpenBSD's `rand()` is random unless seeded with `srand_deterministic`,
/// so every run differs from the fixed sequences of Linux and macOS.
#[test]
fn mtests10_run() {
    for name in bins::MTESTS10.iter().filter(|n| {
        !(cfg!(target_os = "openbsd") && (**n == "mtest_remap" || n.starts_with("mtest_enc")))
    }) {
        let env = TestEnv::with_name("testdb").expect("temp env");
        let bin = bins::mtest10(name).expect("built");
        let out = run_tool_any_status(bin, &[], Some(env.root()), None).expect("run mtest");
        assert!(out.success(), "{out}");
        let path = env.path().as_os_str().to_owned();
        let stat = run(bins::mdb_stat10(), &[&path], None);
        if name.starts_with("mtest_enc") {
            assert!(stat.stderr.contains("MDB_ENV_ENCRYPTION"), "{stat}");
        } else {
            assert!(stat.success(), "{name}: {stat}");
        }
    }
}
