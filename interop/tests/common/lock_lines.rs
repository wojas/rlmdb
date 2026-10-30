//! The Rust side of the lock-file lines of `layout_dump` (LMDB 0.9) and
//! `layout_dump10` (LMDB 1.0): every offset, size and constant of
//! `MDB_txninfo`, `MDB_txbody`, `MDB_reader` and `MDB_rxbody`, as the
//! descriptor the library uses (`rlmdb::format::lockfile::LockLayout`) has
//! it. Included by `layout.rs` (version 1) and `lockfile10.rs` (both
//! versions) with `#[path]`.

#![allow(dead_code)] // each test file uses a subset

use interop::tools::Layout;
use rlmdb::format::lockfile::{
    LockLayout, LockObjects, READER_PID, READER_SIZE, READER_TID, READER_TXNID, TXNINFO_FORMAT,
    TXNINFO_MAGIC,
};

/// `MDB_LOCK_VERSION_BITS` of LMDB 1.0: `mti_format` holds the version in
/// its low 12 bits and `MDB_lock_desc` above them.
const LOCK_VERSION_BITS: u32 = 12;

/// `true` for a `layout_dump` line that describes the lock file.
pub fn is_lock_line(name: &str) -> bool {
    [
        "MDB_txbody",
        "MDB_txninfo",
        "MDB_reader",
        "MDB_rxbody",
        "MDB_LOCK_",
        "MDB_lock_desc",
        "MNAME_LEN",
    ]
    .iter()
    .any(|p| name.contains(p))
}

/// Lock-file lines without a counterpart in the descriptor, and why.
pub fn not_compared(name: &str, layout: &LockLayout) -> Option<&'static str> {
    if name.starts_with("alignof(") {
        return Some("Rust decodes byte slices and has no struct alignment");
    }
    if name == "MNAME_LEN" && layout.objects == LockObjects::SemaphoreHash {
        return Some(
            "LMDB 1.0 defines it as sizeof(pthread_mutex_t) even with named semaphores, \
             where nothing of that size is in the lock file",
        );
    }
    None
}

/// The C names of the reader and writer lock fields.
fn lock_field_names(layout: &LockLayout) -> (&'static str, Option<&'static str>) {
    match layout.objects {
        LockObjects::PthreadMutexes => ("mtb_rmutex", Some("mt2_wmutex")),
        LockObjects::SemaphoreNames => ("mtb_rmname", Some("mt2_wmname")),
        LockObjects::SemaphoreHash => ("mtb_mutexid", None),
    }
}

/// Every lock-file line `layout_dump` (version 1) or `layout_dump10`
/// (version 2) prints on this platform, with the value the descriptor
/// gives.
pub fn rust_lock_lines(layout: &LockLayout) -> Vec<(String, u64)> {
    let (rlock, wlock) = lock_field_names(layout);
    let mut v: Vec<(String, usize)> = vec![
        ("MDB_LOCK_VERSION".into(), layout.version as usize),
        ("MDB_LOCK_FORMAT".into(), layout.lock_format as usize),
        // MDB_rxbody / MDB_reader
        ("sizeof(MDB_rxbody)".into(), READER_TID + 8),
        ("sizeof(MDB_reader)".into(), READER_SIZE),
        // MDB_txbody
        ("sizeof(MDB_txbody)".into(), layout.txbody_size),
        // MDB_txninfo: one reader slot is part of the C struct.
        ("sizeof(MDB_txninfo)".into(), layout.readers + READER_SIZE),
        ("offsetof(MDB_txninfo.mt1)".into(), 0),
        ("sizeof(MDB_txninfo.mt1)".into(), layout.header_size),
        ("offsetof(MDB_txninfo.mti_readers)".into(), layout.readers),
        ("sizeof(MDB_txninfo.mti_readers)".into(), READER_SIZE),
        (
            "offsetof(MDB_txninfo.mti_readers[1])".into(),
            layout.reader_offset(1),
        ),
        ("sizeof(MDB_txninfo.mti_readers[1])".into(), READER_SIZE),
    ];
    if layout.objects != LockObjects::SemaphoreHash {
        v.push(("MNAME_LEN".into(), layout.rlock_size));
    }
    if layout.version >= 2 {
        let desc = layout.lock_format >> LOCK_VERSION_BITS;
        v.extend([
            ("MDB_LOCK_VERSION_BITS".into(), LOCK_VERSION_BITS as usize),
            ("MDB_lock_desc".into(), desc as usize),
            // 270 * (MDB_LOCK_TYPE % 120) plus terms below 270, in the low
            // 15 bits of MDB_lock_desc.
            ("MDB_LOCK_TYPE".into(), (desc & 0x7fff) as usize / 270),
        ]);
    }
    if let (Some(off), Some(name)) = (layout.wlock, wlock) {
        v.extend([
            ("offsetof(MDB_txninfo.mt2)".into(), off),
            ("sizeof(MDB_txninfo.mt2)".into(), layout.readers - off),
            (format!("offsetof(MDB_txninfo.mt2.{name})"), off),
            // The writer lock has the reader lock's size (MNAME_LEN).
            (format!("sizeof(MDB_txninfo.mt2.{name})"), layout.rlock_size),
        ]);
    }
    // The reader slot fields, under both C names.
    for prefix in ["MDB_rxbody", "MDB_reader.mru.mrx"] {
        for (field, off, size) in [
            ("mrb_txnid", READER_TXNID, 8),
            ("mrb_pid", READER_PID, 4),
            ("mrb_tid", READER_TID, 8),
        ] {
            v.push((format!("offsetof({prefix}.{field})"), off));
            v.push((format!("sizeof({prefix}.{field})"), size));
        }
    }
    // The header fields, under MDB_txbody and MDB_txninfo.mt1.mtb.
    for prefix in ["MDB_txbody", "MDB_txninfo.mt1.mtb"] {
        for (field, off, size) in [
            ("mtb_magic", TXNINFO_MAGIC, 4),
            ("mtb_format", TXNINFO_FORMAT, 4),
            (rlock, layout.rlock, layout.rlock_size),
            ("mtb_txnid", layout.txnid, 8),
            ("mtb_numreaders", layout.num_readers, 4),
        ] {
            v.push((format!("offsetof({prefix}.{field})"), off));
            v.push((format!("sizeof({prefix}.{field})"), size));
        }
    }
    v.into_iter().map(|(n, x)| (n, x as u64)).collect()
}

/// Compares every lock-file line of `c` (a `layout_dump` output) with the
/// descriptor `layout`; returns the differences, one per line.
pub fn lock_line_mismatches(c: &Layout, layout: &LockLayout) -> Vec<String> {
    let rust = rust_lock_lines(layout);
    let mut mismatches = Vec::new();
    for (name, value) in c.values.iter().filter(|(n, _)| is_lock_line(n)) {
        if not_compared(name, layout).is_some() {
            continue;
        }
        match rust.iter().find(|(n, _)| n == name) {
            Some((_, r)) if r == value => {}
            Some((_, r)) => mismatches.push(format!("{name}: C {value}, Rust {r}")),
            None => mismatches.push(format!("{name}: C {value}, no Rust counterpart")),
        }
    }
    for (name, _) in &rust {
        if !c.values.contains_key(name) {
            mismatches.push(format!("{name}: not printed by layout_dump"));
        }
    }
    mismatches
}
