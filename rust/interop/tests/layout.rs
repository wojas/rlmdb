//! The C build's struct layout (from `layout_dump`) against the on-disk and
//! lock-file format described in `rust/DESIGN.md`.
//!
//! These numbers are what the Rust implementation must reproduce; stage 2
//! also compares them with `lmdb::format::layout` (feature `with-lmdb`).

use interop::tools::{c_layout, Layout};

fn layout() -> &'static Layout {
    c_layout().expect("layout_dump")
}

fn get(name: &str) -> u64 {
    layout().get(name).expect(name)
}

#[test]
fn prints_layout_summary() {
    let l = layout();
    println!(
        "lock_mode={} os_page_size={} sizeof(pthread_mutex_t)={} mti_readers={}",
        l.lock_mode,
        get("os_page_size"),
        get("sizeof(pthread_mutex_t)"),
        get("offsetof(MDB_txninfo.mti_readers)")
    );
}

#[test]
fn constants() {
    assert_eq!(get("MDB_MAGIC"), 0xBEEF_C0DE);
    assert_eq!(get("MDB_DATA_VERSION"), 1);
    assert_eq!(get("MDB_LOCK_VERSION"), 1);
    assert_eq!(get("MDB_LOCK_FORMAT"), 0x10001);
    assert_eq!(get("MDB_PIDLOCK"), 1);
    assert_eq!(get("MDB_DEVEL"), 0);
    assert_eq!(get("PAGEBASE"), 0);
    assert_eq!(get("MDB_MAXKEYSIZE"), 511);
    assert_eq!(get("MDB_MINKEYS"), 2);
    assert_eq!(get("CACHELINE"), 64);
    assert_eq!(get("DEFAULT_READERS"), 126);
    assert_eq!(get("DEFAULT_MAPSIZE"), 1 << 20);
    assert_eq!(get("MAX_PAGESIZE"), 0x8000);
    assert_eq!(get("NUM_METAS"), 2);
    assert_eq!(get("CORE_DBS"), 2);
    assert_eq!(get("P_INVALID"), u64::MAX);
    assert_eq!(get("PGNO_TOPWORD"), 32);
}

#[test]
fn flags() {
    for (name, v) in [
        ("P_BRANCH", 0x01),
        ("P_LEAF", 0x02),
        ("P_OVERFLOW", 0x04),
        ("P_META", 0x08),
        ("P_DIRTY", 0x10),
        ("P_LEAF2", 0x20),
        ("P_SUBP", 0x40),
        ("P_LOOSE", 0x4000),
        ("P_KEEP", 0x8000),
        ("F_BIGDATA", 0x01),
        ("F_SUBDATA", 0x02),
        ("F_DUPDATA", 0x04),
    ] {
        assert_eq!(get(name), v, "{name}");
    }
}

#[test]
fn scalar_types_are_64_bit() {
    for t in ["pgno_t", "txnid_t", "size_t", "pthread_t"] {
        assert_eq!(get(&format!("sizeof({t})")), 8, "{t}");
    }
    assert_eq!(get("sizeof(indx_t)"), 2);
    assert_eq!(get("sizeof(pid_t)"), 4);
    assert_eq!(get("sizeof(unsigned int)"), 4);
}

#[test]
fn page_header() {
    assert_eq!(get("PAGEHDRSZ"), 16);
    assert_eq!(get("sizeof(MDB_page)"), 16);
    for (field, off, size) in [
        ("mp_p.p_pgno", 0, 8),
        ("mp_pad", 8, 2),
        ("mp_flags", 10, 2),
        ("mp_pb.pb.pb_lower", 12, 2),
        ("mp_pb.pb.pb_upper", 14, 2),
        ("mp_pb.pb_pages", 12, 4),
    ] {
        assert_eq!(get(&format!("offsetof(MDB_page.{field})")), off, "{field}");
        assert_eq!(get(&format!("sizeof(MDB_page.{field})")), size, "{field}");
    }
    assert_eq!(get("offsetof(MDB_page.mp_ptrs)"), 16);
}

#[test]
fn node() {
    assert_eq!(get("NODESIZE"), 8);
    assert_eq!(get("offsetof(MDB_node.mn_data)"), 8);
    assert_eq!(get("alignof(MDB_node)"), 2);
    // Little-endian hosts: mn_lo first. (Big-endian swaps lo and hi.)
    for (field, off) in [("mn_lo", 0), ("mn_hi", 2), ("mn_flags", 4), ("mn_ksize", 6)] {
        assert_eq!(get(&format!("offsetof(MDB_node.{field})")), off, "{field}");
        assert_eq!(get(&format!("sizeof(MDB_node.{field})")), 2, "{field}");
    }
}

#[test]
fn db() {
    assert_eq!(get("sizeof(MDB_db)"), 48);
    for (field, off, size) in [
        ("md_pad", 0, 4),
        ("md_flags", 4, 2),
        ("md_depth", 6, 2),
        ("md_branch_pages", 8, 8),
        ("md_leaf_pages", 16, 8),
        ("md_overflow_pages", 24, 8),
        ("md_entries", 32, 8),
        ("md_root", 40, 8),
    ] {
        assert_eq!(get(&format!("offsetof(MDB_db.{field})")), off, "{field}");
        assert_eq!(get(&format!("sizeof(MDB_db.{field})")), size, "{field}");
    }
}

#[test]
fn meta() {
    assert_eq!(get("sizeof(MDB_meta)"), 136);
    assert_eq!(get("offsetof(MDB_metabuf.mb_metabuf.mm_meta)"), 16);
    for (field, off, size) in [
        ("mm_magic", 0, 4),
        ("mm_version", 4, 4),
        ("mm_address", 8, 8),
        ("mm_mapsize", 16, 8),
        ("mm_dbs[0]", 24, 48),
        ("mm_dbs[1]", 72, 48),
        ("mm_last_pg", 120, 8),
        ("mm_txnid", 128, 8),
    ] {
        assert_eq!(get(&format!("offsetof(MDB_meta.{field})")), off, "{field}");
        assert_eq!(get(&format!("sizeof(MDB_meta.{field})")), size, "{field}");
    }
}

#[test]
fn reader_slot() {
    assert_eq!(get("sizeof(MDB_reader)"), 64);
    assert_eq!(get("sizeof(MDB_rxbody)"), 24);
    for (field, off, size) in [("mrb_txnid", 0, 8), ("mrb_pid", 8, 4), ("mrb_tid", 16, 8)] {
        assert_eq!(
            get(&format!("offsetof(MDB_reader.mru.mrx.{field})")),
            off,
            "{field}"
        );
        assert_eq!(
            get(&format!("offsetof(MDB_rxbody.{field})")),
            off,
            "{field}"
        );
        assert_eq!(get(&format!("sizeof(MDB_rxbody.{field})")), size, "{field}");
    }
}

/// Round `n` up to a multiple of the cache line, as `MDB_txninfo` does.
fn cache_pad(n: u64) -> u64 {
    n.div_ceil(64) * 64
}

#[test]
fn lock_file_header() {
    let l = layout();
    let mutex = get("sizeof(pthread_mutex_t)");
    assert_eq!(get("offsetof(MDB_txninfo.mt1.mtb.mtb_magic)"), 0);
    assert_eq!(get("offsetof(MDB_txninfo.mt1.mtb.mtb_format)"), 4);
    assert_eq!(get("offsetof(MDB_txninfo.mt1)"), 0);
    // The shared-lock field: semaphore name or the mutex itself.
    let (lock_field_size, txnid_off) = match l.lock_mode.as_str() {
        "posix_sem" => {
            assert_eq!(get("MNAME_LEN"), 32);
            assert_eq!(get("offsetof(MDB_txninfo.mt1.mtb.mtb_rmname)"), 8);
            assert_eq!(get("offsetof(MDB_txninfo.mt2.mt2_wmname)"), 64);
            (32, 40)
        }
        "posix_mutex" => {
            assert_eq!(get("MNAME_LEN"), mutex);
            let align = get("alignof(pthread_mutex_t)");
            let rmutex = get("offsetof(MDB_txninfo.mt1.mtb.mtb_rmutex)");
            assert_eq!(rmutex, 8u64.div_ceil(align) * align);
            (mutex, (rmutex + mutex).div_ceil(8) * 8)
        }
        other => panic!("unexpected lock mode {other}"),
    };
    assert_eq!(get("offsetof(MDB_txninfo.mt1.mtb.mtb_txnid)"), txnid_off);
    assert_eq!(
        get("offsetof(MDB_txninfo.mt1.mtb.mtb_numreaders)"),
        txnid_off + 8
    );
    let mt1 = cache_pad(txnid_off + 8 + 4);
    let mt2 = cache_pad(lock_field_size);
    assert_eq!(get("sizeof(MDB_txninfo.mt1)"), mt1);
    assert_eq!(get("offsetof(MDB_txninfo.mt2)"), mt1);
    assert_eq!(get("sizeof(MDB_txninfo.mt2)"), mt2);
    assert_eq!(get("offsetof(MDB_txninfo.mti_readers)"), mt1 + mt2);
    assert_eq!(get("offsetof(MDB_txninfo.mti_readers[1])"), mt1 + mt2 + 64);
    if cfg!(target_os = "macos") {
        assert_eq!(l.lock_mode, "posix_sem");
        assert_eq!(get("offsetof(MDB_txninfo.mti_readers)"), 128);
    }
    if cfg!(target_os = "linux") {
        assert_eq!(l.lock_mode, "posix_mutex");
        assert_eq!(get("MDB_ROBUST_SUPPORTED"), 1);
    }
}

/// Stage 2: the Rust library's layout constants must equal the C build's.
///
/// Enable with `cargo test -p interop --features with-lmdb` once
/// `lmdb::format::layout` exists; adjust the constant names below to the ones
/// the library uses.
#[cfg(feature = "with-lmdb")]
#[test]
fn rust_layout_matches_c() {
    use lmdb::format::layout as rs;
    let pairs: &[(&str, u64)] = &[
        ("PAGEHDRSZ", rs::PAGE_HDR_SIZE as u64),
        ("NODESIZE", rs::NODE_SIZE as u64),
        ("sizeof(MDB_db)", rs::DB_SIZE as u64),
        ("sizeof(MDB_meta)", rs::META_SIZE as u64),
        ("sizeof(MDB_reader)", rs::READER_SIZE as u64),
        (
            "offsetof(MDB_txninfo.mti_readers)",
            rs::READERS_OFFSET as u64,
        ),
        (
            "offsetof(MDB_txninfo.mt1.mtb.mtb_txnid)",
            rs::TXNINFO_TXNID_OFFSET as u64,
        ),
        (
            "offsetof(MDB_txninfo.mt1.mtb.mtb_numreaders)",
            rs::TXNINFO_NUMREADERS_OFFSET as u64,
        ),
        (
            "offsetof(MDB_reader.mru.mrx.mrb_pid)",
            rs::READER_PID_OFFSET as u64,
        ),
        (
            "offsetof(MDB_reader.mru.mrx.mrb_tid)",
            rs::READER_TID_OFFSET as u64,
        ),
        ("MDB_LOCK_FORMAT", u64::from(rs::LOCK_FORMAT)),
        ("MDB_MAGIC", u64::from(rs::MAGIC)),
        ("MDB_DATA_VERSION", u64::from(rs::DATA_VERSION)),
        ("MDB_MAXKEYSIZE", rs::MAX_KEY_SIZE as u64),
    ];
    for (name, rust) in pairs {
        assert_eq!(get(name), *rust, "{name}");
    }
}
