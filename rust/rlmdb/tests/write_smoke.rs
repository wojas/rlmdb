use rlmdb::{DatabaseFlags, Environment, EnvironmentFlags, Transaction, WriteFlags};

#[test]
fn put_commit_read() {
    let dir = tempfile::tempdir().unwrap();
    let env = Environment::builder()
        .map_size(64 << 20)
        .max_dbs(4)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    for i in 0..5000u32 {
        let k = format!("key{:08}", (i * 7919) % 5000);
        let v = format!("value-{i}");
        txn.put(db, k.as_bytes(), v.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    assert_eq!(txn.stat(db).unwrap().entries, 5000);
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    let mut n = 0;
    let mut prev: Vec<u8> = Vec::new();
    while let Some((k, _v)) = c.next().unwrap() {
        assert!(k > &prev[..]);
        prev = k.to_vec();
        n += 1;
    }
    assert_eq!(n, 5000);
}
