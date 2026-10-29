//! The C build's struct layout (from `layout_dump`) against the on-disk and
//! lock-file format described in `rust/DESIGN.md`.
//!
//! These numbers are what the Rust implementation must reproduce;
//! `rust_layout_matches_c` compares every one of them with the constants of
//! the `lmdb` crate (`lmdb_core::consts`, `lmdb_core::format::layout`).

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

/// Every value `layout_dump` prints that has a counterpart in the Rust
/// library, as the library sees it: constants from `lmdb_core::consts` and
/// `lmdb_core::format::layout`, and sizes of the fields of its decoded structs.
/// On Linux the lock-file offsets depend on `sizeof(pthread_mutex_t)`, which
/// both sides take from the platform's C headers (`libc` on the Rust side).
fn rust_layout() -> Vec<(String, u64)> {
    use lmdb_core::consts as c;
    use lmdb_core::format::layout as l;
    use lmdb_core::format::{lockfile, node, page, Db, Meta, PageRef};
    use std::mem::{align_of, size_of, size_of_val};

    let sem = lockfile::USES_NAMED_SEMAPHORES;
    let mutex_align = if sem {
        1
    } else {
        align_of::<libc::pthread_mutex_t>()
    };
    let (rlock, wlock) = if sem {
        ("mtb_rmname", "mt2_wmname")
    } else {
        ("mtb_rmutex", "mt2_wmutex")
    };
    let hdr = [0u8; l::PAGE_HDR_SIZE];
    let pg = PageRef::new(&hdr).unwrap();
    let nd = lmdb_core::format::NodeRef::new(&hdr).unwrap();
    let db = Db::empty(0);
    let meta = Meta::default();
    let reader = lmdb_core::ReaderInfo {
        pid: 0,
        thread: 0,
        txnid: None,
    };
    let txbody = (l::TXNINFO_NUMREADERS + 4).next_multiple_of(8.max(mutex_align));

    let mut v: Vec<(String, usize)> = vec![
        ("MDB_ROBUST_SUPPORTED".into(), usize::from(!sem)),
        ("MDB_MAGIC".into(), c::MDB_MAGIC as usize),
        ("MDB_DATA_VERSION".into(), c::MDB_DATA_VERSION as usize),
        ("MDB_LOCK_VERSION".into(), c::MDB_LOCK_VERSION as usize),
        ("MDB_LOCK_FORMAT".into(), l::LOCK_FORMAT as usize),
        ("MDB_PIDLOCK".into(), c::MDB_PIDLOCK as usize),
        ("PAGEHDRSZ".into(), l::PAGE_HDR_SIZE),
        ("NODESIZE".into(), l::NODE_SIZE),
        ("MDB_MAXKEYSIZE".into(), c::MDB_MAXKEYSIZE),
        ("MDB_MINKEYS".into(), c::MDB_MINKEYS),
        ("CACHELINE".into(), c::CACHELINE),
        ("MNAME_LEN".into(), l::MNAME_LEN),
        ("DEFAULT_READERS".into(), c::DEFAULT_READERS as usize),
        ("DEFAULT_MAPSIZE".into(), c::DEFAULT_MAPSIZE),
        ("MAX_PAGESIZE".into(), c::MAX_PAGESIZE),
        ("NUM_METAS".into(), c::NUM_METAS),
        ("CORE_DBS".into(), c::CORE_DBS as usize),
        ("P_INVALID".into(), c::P_INVALID as usize),
        ("P_BRANCH".into(), page::P_BRANCH.into()),
        ("P_LEAF".into(), page::P_LEAF.into()),
        ("P_OVERFLOW".into(), page::P_OVERFLOW.into()),
        ("P_META".into(), page::P_META.into()),
        ("P_DIRTY".into(), page::P_DIRTY.into()),
        ("P_LEAF2".into(), page::P_LEAF2.into()),
        ("P_SUBP".into(), page::P_SUBP.into()),
        ("P_LOOSE".into(), page::P_LOOSE.into()),
        ("P_KEEP".into(), page::P_KEEP.into()),
        ("F_BIGDATA".into(), node::F_BIGDATA.into()),
        ("F_SUBDATA".into(), node::F_SUBDATA.into()),
        ("F_DUPDATA".into(), node::F_DUPDATA.into()),
        // Scalar types: the Rust field types that hold them.
        ("sizeof(pgno_t)".into(), size_of_val(&db.root)),
        ("alignof(pgno_t)".into(), align_of::<u64>()),
        ("sizeof(txnid_t)".into(), size_of_val(&meta.txnid)),
        ("alignof(txnid_t)".into(), align_of::<u64>()),
        ("sizeof(indx_t)".into(), size_of_val(&pg.lower())),
        ("alignof(indx_t)".into(), align_of::<u16>()),
        ("sizeof(size_t)".into(), size_of::<usize>()),
        ("alignof(size_t)".into(), align_of::<usize>()),
        ("sizeof(pid_t)".into(), size_of_val(&reader.pid)),
        ("alignof(pid_t)".into(), align_of::<libc::pid_t>()),
        ("sizeof(pthread_t)".into(), size_of_val(&reader.thread)),
        ("alignof(pthread_t)".into(), align_of::<libc::pthread_t>()),
        (
            "sizeof(pthread_mutex_t)".into(),
            size_of::<libc::pthread_mutex_t>(),
        ),
        (
            "alignof(pthread_mutex_t)".into(),
            align_of::<libc::pthread_mutex_t>(),
        ),
        ("sizeof(unsigned int)".into(), size_of::<std::ffi::c_uint>()),
        (
            "alignof(unsigned int)".into(),
            align_of::<std::ffi::c_uint>(),
        ),
        // The process-local lock handle: `sem_t *`, or the mutex itself.
        (
            "sizeof(mdb_mutex_t)".into(),
            if sem {
                size_of::<usize>()
            } else {
                l::MNAME_LEN
            },
        ),
        (
            "alignof(mdb_mutex_t)".into(),
            if sem {
                align_of::<usize>()
            } else {
                mutex_align
            },
        ),
        // MDB_page
        ("sizeof(MDB_page)".into(), l::PAGE_HDR_SIZE),
        ("offsetof(MDB_page.mp_p.p_pgno)".into(), l::PAGE_PGNO),
        (
            "sizeof(MDB_page.mp_p.p_pgno)".into(),
            size_of_val(&pg.pgno()),
        ),
        ("offsetof(MDB_page.mp_pad)".into(), l::PAGE_PAD),
        ("sizeof(MDB_page.mp_pad)".into(), size_of_val(&pg.pad())),
        ("offsetof(MDB_page.mp_flags)".into(), l::PAGE_FLAGS),
        ("sizeof(MDB_page.mp_flags)".into(), size_of_val(&pg.flags())),
        ("offsetof(MDB_page.mp_pb.pb.pb_lower)".into(), l::PAGE_LOWER),
        (
            "sizeof(MDB_page.mp_pb.pb.pb_lower)".into(),
            size_of_val(&pg.lower()),
        ),
        ("offsetof(MDB_page.mp_pb.pb.pb_upper)".into(), l::PAGE_UPPER),
        (
            "sizeof(MDB_page.mp_pb.pb.pb_upper)".into(),
            size_of_val(&pg.upper()),
        ),
        (
            "offsetof(MDB_page.mp_pb.pb_pages)".into(),
            l::PAGE_OVERFLOW_PAGES,
        ),
        (
            "sizeof(MDB_page.mp_pb.pb_pages)".into(),
            size_of_val(&pg.overflow_pages()),
        ),
        ("offsetof(MDB_page.mp_ptrs)".into(), l::PAGE_PTRS),
        // MDB_node
        ("offsetof(MDB_node.mn_lo)".into(), l::NODE_LO),
        ("sizeof(MDB_node.mn_lo)".into(), size_of_val(&nd.lo())),
        ("offsetof(MDB_node.mn_hi)".into(), l::NODE_HI),
        ("sizeof(MDB_node.mn_hi)".into(), size_of_val(&nd.hi())),
        ("offsetof(MDB_node.mn_flags)".into(), l::NODE_FLAGS),
        ("sizeof(MDB_node.mn_flags)".into(), size_of_val(&nd.flags())),
        ("offsetof(MDB_node.mn_ksize)".into(), l::NODE_KSIZE),
        ("sizeof(MDB_node.mn_ksize)".into(), 2),
        ("offsetof(MDB_node.mn_data)".into(), l::NODE_DATA),
        // MDB_db
        ("sizeof(MDB_db)".into(), l::DB_SIZE),
        ("offsetof(MDB_db.md_pad)".into(), l::DB_PAD),
        ("sizeof(MDB_db.md_pad)".into(), size_of_val(&db.pad)),
        ("offsetof(MDB_db.md_flags)".into(), l::DB_FLAGS),
        ("sizeof(MDB_db.md_flags)".into(), size_of_val(&db.flags)),
        ("offsetof(MDB_db.md_depth)".into(), l::DB_DEPTH),
        ("sizeof(MDB_db.md_depth)".into(), size_of_val(&db.depth)),
        (
            "offsetof(MDB_db.md_branch_pages)".into(),
            l::DB_BRANCH_PAGES,
        ),
        (
            "sizeof(MDB_db.md_branch_pages)".into(),
            size_of_val(&db.branch_pages),
        ),
        ("offsetof(MDB_db.md_leaf_pages)".into(), l::DB_LEAF_PAGES),
        (
            "sizeof(MDB_db.md_leaf_pages)".into(),
            size_of_val(&db.leaf_pages),
        ),
        (
            "offsetof(MDB_db.md_overflow_pages)".into(),
            l::DB_OVERFLOW_PAGES,
        ),
        (
            "sizeof(MDB_db.md_overflow_pages)".into(),
            size_of_val(&db.overflow_pages),
        ),
        ("offsetof(MDB_db.md_entries)".into(), l::DB_ENTRIES),
        ("sizeof(MDB_db.md_entries)".into(), size_of_val(&db.entries)),
        ("offsetof(MDB_db.md_root)".into(), l::DB_ROOT),
        ("sizeof(MDB_db.md_root)".into(), size_of_val(&db.root)),
        // MDB_meta
        ("sizeof(MDB_meta)".into(), l::META_SIZE),
        ("offsetof(MDB_meta.mm_magic)".into(), l::META_MAGIC),
        ("sizeof(MDB_meta.mm_magic)".into(), size_of_val(&meta.magic)),
        ("offsetof(MDB_meta.mm_version)".into(), l::META_VERSION),
        (
            "sizeof(MDB_meta.mm_version)".into(),
            size_of_val(&meta.version),
        ),
        ("offsetof(MDB_meta.mm_address)".into(), l::META_ADDRESS),
        (
            "sizeof(MDB_meta.mm_address)".into(),
            size_of_val(&meta.address),
        ),
        ("offsetof(MDB_meta.mm_mapsize)".into(), l::META_MAPSIZE),
        (
            "sizeof(MDB_meta.mm_mapsize)".into(),
            size_of_val(&meta.map_size),
        ),
        ("offsetof(MDB_meta.mm_dbs)".into(), l::META_FREE_DB),
        (
            "sizeof(MDB_meta.mm_dbs)".into(),
            meta.dbs.len() * l::DB_SIZE,
        ),
        ("offsetof(MDB_meta.mm_dbs[0])".into(), l::META_FREE_DB),
        ("sizeof(MDB_meta.mm_dbs[0])".into(), l::DB_SIZE),
        ("offsetof(MDB_meta.mm_dbs[1])".into(), l::META_MAIN_DB),
        ("sizeof(MDB_meta.mm_dbs[1])".into(), l::DB_SIZE),
        ("offsetof(MDB_meta.mm_last_pg)".into(), l::META_LAST_PGNO),
        (
            "sizeof(MDB_meta.mm_last_pg)".into(),
            size_of_val(&meta.last_pgno),
        ),
        ("offsetof(MDB_meta.mm_txnid)".into(), l::META_TXNID),
        ("sizeof(MDB_meta.mm_txnid)".into(), size_of_val(&meta.txnid)),
        (
            "offsetof(MDB_metabuf.mb_metabuf.mm_meta)".into(),
            l::META_OFFSET_IN_PAGE,
        ),
        // MDB_reader / MDB_rxbody
        (
            "sizeof(MDB_rxbody)".into(),
            l::READER_TID + size_of_val(&reader.thread),
        ),
        ("sizeof(MDB_reader)".into(), l::READER_SIZE),
        // MDB_txninfo / MDB_txbody
        ("sizeof(MDB_txbody)".into(), txbody),
        ("offsetof(MDB_txbody.mtb_magic)".into(), l::TXNINFO_MAGIC),
        ("sizeof(MDB_txbody.mtb_magic)".into(), 4),
        ("offsetof(MDB_txbody.mtb_format)".into(), l::TXNINFO_FORMAT),
        (
            "sizeof(MDB_txbody.mtb_format)".into(),
            size_of_val(&l::LOCK_FORMAT),
        ),
        (format!("offsetof(MDB_txbody.{rlock})"), l::TXNINFO_RMUTEX),
        (format!("sizeof(MDB_txbody.{rlock})"), l::MNAME_LEN),
        ("offsetof(MDB_txbody.mtb_txnid)".into(), l::TXNINFO_TXNID),
        (
            "sizeof(MDB_txbody.mtb_txnid)".into(),
            size_of_val(&meta.txnid),
        ),
        (
            "offsetof(MDB_txbody.mtb_numreaders)".into(),
            l::TXNINFO_NUMREADERS,
        ),
        ("sizeof(MDB_txbody.mtb_numreaders)".into(), size_of::<u32>()),
        // One reader slot is part of the C struct.
        (
            "sizeof(MDB_txninfo)".into(),
            l::READERS_OFFSET + l::READER_SIZE,
        ),
        ("offsetof(MDB_txninfo.mt1)".into(), 0),
        ("sizeof(MDB_txninfo.mt1)".into(), l::TXNINFO_HEADER_SIZE),
        ("offsetof(MDB_txninfo.mt2)".into(), l::TXNINFO_WMUTEX),
        (
            "sizeof(MDB_txninfo.mt2)".into(),
            l::READERS_OFFSET - l::TXNINFO_WMUTEX,
        ),
        (
            format!("offsetof(MDB_txninfo.mt2.{wlock})"),
            l::TXNINFO_WMUTEX,
        ),
        (format!("sizeof(MDB_txninfo.mt2.{wlock})"), l::MNAME_LEN),
        (
            "offsetof(MDB_txninfo.mti_readers)".into(),
            l::READERS_OFFSET,
        ),
        ("sizeof(MDB_txninfo.mti_readers)".into(), l::READER_SIZE),
        (
            "offsetof(MDB_txninfo.mti_readers[1])".into(),
            lockfile::reader_offset(1),
        ),
        ("sizeof(MDB_txninfo.mti_readers[1])".into(), l::READER_SIZE),
    ];
    // The reader slot fields, under both C names.
    for prefix in ["MDB_rxbody", "MDB_reader.mru.mrx"] {
        v.extend([
            (format!("offsetof({prefix}.mrb_txnid)"), l::READER_TXNID),
            (
                format!("sizeof({prefix}.mrb_txnid)"),
                size_of_val(&meta.txnid),
            ),
            (format!("offsetof({prefix}.mrb_pid)"), l::READER_PID),
            (
                format!("sizeof({prefix}.mrb_pid)"),
                size_of_val(&reader.pid),
            ),
            (format!("offsetof({prefix}.mrb_tid)"), l::READER_TID),
            (
                format!("sizeof({prefix}.mrb_tid)"),
                size_of_val(&reader.thread),
            ),
        ]);
    }
    // The lock-file header fields, also under MDB_txninfo.mt1.mtb.
    for field in [
        "mtb_magic",
        "mtb_format",
        rlock,
        "mtb_txnid",
        "mtb_numreaders",
    ] {
        let off = v
            .iter()
            .find(|(n, _)| *n == format!("offsetof(MDB_txbody.{field})"))
            .map(|(_, x)| *x)
            .unwrap();
        let size = v
            .iter()
            .find(|(n, _)| *n == format!("sizeof(MDB_txbody.{field})"))
            .map(|(_, x)| *x)
            .unwrap();
        v.push((format!("offsetof(MDB_txninfo.mt1.mtb.{field})"), off));
        v.push((format!("sizeof(MDB_txninfo.mt1.mtb.{field})"), size));
    }
    v.into_iter().map(|(n, x)| (n, x as u64)).collect()
}

/// `layout_dump` values without a counterpart in the Rust library, and why.
const NOT_COMPARED: &[(&str, &str)] = &[
    ("MDB_DEVEL", "development data format; the Rust crate implements only the release format (checked to be 0 in `constants`)"),
    ("PAGEBASE", "always 0 in 0.9: node offsets are relative to the page start (`PageRef::node`)"),
    ("PGNO_TOPWORD", "branch node page numbers are `lo | hi << 16 | flags << 32` (`NodeRef::child_pgno`)"),
    ("os_page_size", "not a layout value; `create_stat` checks the page size of new environments"),
    ("sizeof(MDB_node)", "includes C's one-byte `mn_data[1]` placeholder and padding; the header is NODESIZE"),
    ("alignof(MDB_page)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_node)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_db)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_meta)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_rxbody)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_reader)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_txbody)", "Rust decodes byte slices and has no struct alignment"),
    ("alignof(MDB_txninfo)", "Rust decodes byte slices and has no struct alignment"),
];

/// The Rust library's layout constants equal the C build's, for every value
/// `layout_dump` prints (on macOS and on Linux, where the lock file embeds
/// `pthread_mutex_t`s). A value `layout_dump` starts printing must be added
/// to [`rust_layout`] or [`NOT_COMPARED`].
#[test]
fn rust_layout_matches_c() {
    let l = layout();
    let rust = rust_layout();
    let expected_mode = if lmdb_core::format::lockfile::USES_NAMED_SEMAPHORES {
        "posix_sem"
    } else {
        "posix_mutex"
    };
    assert_eq!(l.lock_mode, expected_mode, "lock mode");
    let mut mismatches = Vec::new();
    for (name, c) in &l.values {
        if NOT_COMPARED.iter().any(|(n, _)| n == name) {
            continue;
        }
        match rust.iter().find(|(n, _)| n == name) {
            Some((_, r)) if r == c => {}
            Some((_, r)) => mismatches.push(format!("{name}: C {c}, Rust {r}")),
            None => mismatches.push(format!(
                "{name}: C {c}, not compared (add it to rust_layout)"
            )),
        }
    }
    for (name, _) in &rust {
        if !l.values.contains_key(name) {
            mismatches.push(format!("{name}: not printed by layout_dump"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "layout differences:\n{}",
        mismatches.join("\n")
    );
}
