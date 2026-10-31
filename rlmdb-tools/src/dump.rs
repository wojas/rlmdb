//! `mdb_dump`: write databases in the portable dump format that `mdb_load`
//! reads (`tests/lmdb-c/0.9/mdb_dump.c`; the format is described in
//! [`crate::dumpfmt`]).
//!
//! ```text
//! mdb_dump [-V] [-f output] [-l] [-n] [-p] [-a|-s subdb] dbpath
//! ```
//!
//! Output and messages are those of the C tool of the environment's format.
//! The dump format is the same for LMDB 0.9 and 1.0 environments; for a 1.0
//! one the messages are 1.0's (`mdb_dbi_open`), and `-a`/`-l` list the
//! records of named databases (`mdb_cursor_is_db`; 1.0 stores a name with
//! its NUL, which 0.9's tool would skip).
//!
//! C installs handlers for `SIGPIPE`, `SIGHUP`, `SIGINT` and `SIGTERM` that
//! make the dump stop after the current record with "Interrupted system
//! call". The Rust tool reproduces the `SIGPIPE` case (a closed pipe on
//! standard output), which is the common one; the other signals terminate
//! it immediately (installing handlers would need `unsafe`). Write errors
//! other than a closed pipe, which C ignores, are reported.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

use rlmdb::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, RoTxn, Transaction};

use crate::cfmt::{err_code, io_errno, pointer, strerror};
use crate::common::{
    db_name_of, dbi_open_name, open_db, path_of, report_failed, Tool, EXIT_FAILURE,
};
use crate::dumpfmt::encode_line;
use crate::getopt::{GetOpt, Opt};
use crate::output::{eprint_bytes, Output};

const SYNOPSIS: &str = "[-V] [-f output] [-l] [-n] [-p] [-a|-s subdb] dbpath";

const NOTFOUND: i32 = rlmdb::consts::MDB_NOTFOUND;
const EINTR: i32 = 4;
const EPIPE: i32 = 32;

/// The database flags `mdb_dump` writes as `name=1` header lines, in its
/// order (`dbflags[]` in `mdb_dump.c`; `mdb_load.c` reads the same names).
pub const DB_FLAG_NAMES: [(DatabaseFlags, &str); 6] = [
    (DatabaseFlags::REVERSE_KEY, "reversekey"),
    (DatabaseFlags::DUP_SORT, "dupsort"),
    (DatabaseFlags::INTEGER_KEY, "integerkey"),
    (DatabaseFlags::DUP_FIXED, "dupfixed"),
    (DatabaseFlags::INTEGER_DUP, "integerdup"),
    (DatabaseFlags::REVERSE_DUP, "reversedup"),
];

/// Command line options.
#[derive(Debug, Default)]
struct Options {
    all_dbs: u32,
    list: bool,
    no_sub_dir: bool,
    print: bool,
    subname: Option<Vec<u8>>,
}

/// The `main` of `mdb_dump`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    if args.len() < 2 {
        return tool.usage(SYNOPSIS);
    }
    let mut o = Options::default();
    let mut getopt = GetOpt::new(args, "af:lnps:V");
    while let Some(opt) = getopt.next() {
        match opt {
            Opt::Flag(b'V') => return tool.version(),
            Opt::Flag(c @ (b'l' | b'a')) => {
                if c == b'l' {
                    o.list = true;
                }
                if o.subname.is_some() {
                    return tool.usage(SYNOPSIS);
                }
                o.all_dbs += 1;
            }
            Opt::Arg(b'f', path) => {
                // freopen(optarg, "w", stdout)
                match OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o666)
                    .open(path_of(&path))
                {
                    Ok(file) => tool.out = Output::file(file),
                    Err(e) => {
                        let text = strerror(io_errno(&e));
                        tool.eprint_prog(&[&path, b": reopen: ", text.as_bytes(), b"\n"]);
                        return EXIT_FAILURE;
                    }
                }
            }
            Opt::Flag(b'n') => o.no_sub_dir = true,
            Opt::Flag(b'p') => o.print = true,
            Opt::Arg(b's', name) => {
                if o.all_dbs > 0 {
                    return tool.usage(SYNOPSIS);
                }
                o.subname = Some(name);
            }
            Opt::Invalid(msg) => {
                eprint_bytes(&[&msg]);
                return tool.usage(SYNOPSIS);
            }
            _ => return tool.usage(SYNOPSIS),
        }
    }
    let operands = getopt.operands();
    if operands.len() != 1 {
        return tool.usage(SYNOPSIS);
    }
    let envname = &operands[0];

    let mut builder = Environment::builder();
    if o.all_dbs > 0 || o.subname.is_some() {
        builder.max_dbs(2);
    }
    let mut flags = EnvironmentFlags::READ_ONLY;
    if o.no_sub_dir {
        flags |= EnvironmentFlags::NO_SUB_DIR;
    }
    builder.flags(flags);
    let env = match builder.open_with_permissions(path_of(envname), 0o664) {
        Ok(env) => env,
        Err(e) => {
            report_failed("mdb_env_open", &e);
            return EXIT_FAILURE;
        }
    };
    let txn = match env.begin_ro_txn() {
        Ok(t) => t,
        Err(e) => {
            report_failed("mdb_txn_begin", &e);
            return EXIT_FAILURE;
        }
    };
    let dbi = match open_db(&txn, o.subname.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            report_failed(dbi_open_name(&env), &e);
            return EXIT_FAILURE;
        }
    };

    let rc = if o.all_dbs > 0 {
        dump_all(tool, &env, &txn, dbi, &o, envname)
    } else {
        dump_db(
            &mut tool.out,
            &env,
            &txn,
            dbi,
            o.subname.as_deref(),
            o.print,
        )
    };
    if rc != 0 && rc != NOTFOUND {
        let text = strerror(rc);
        tool.eprint_prog(&[envname, b": ", text.as_bytes(), b"\n"]);
    }
    if rc != 0 {
        EXIT_FAILURE
    } else {
        0
    }
}

/// `-a` / `-l`: dump (or list) every named database of the main DB.
fn dump_all(
    tool: &mut Tool,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    o: &Options,
    envname: &[u8],
) -> i32 {
    let mut cursor = match txn.cursor(dbi) {
        Ok(c) => c,
        Err(e) => {
            report_failed("mdb_cursor_open", &e);
            return err_code(&e);
        }
    };
    let format = env.format();
    let mut count = 0usize;
    let mut rc = loop {
        let key = match cursor.next_nodup() {
            Ok(Some((key, _))) => key,
            Ok(None) => break NOTFOUND,
            Err(e) => break err_code(&e),
        };
        let Some(name) = db_name_of(format, &cursor, key) else {
            continue;
        };
        // Counted even if it turns out not to be a database, as in C.
        count += 1;
        let Ok(db2) = open_db(txn, Some(name)) else {
            continue;
        };
        if o.list {
            tool.out.write(name);
            tool.out.write(b"\n");
        } else {
            let rc = dump_db(&mut tool.out, env, txn, db2, Some(name), o.print);
            if rc != 0 {
                env.close_db(db2);
                break rc;
            }
        }
        env.close_db(db2);
    };
    if count == 0 {
        tool.eprint_prog(&[envname, b" does not contain multiple databases\n"]);
        rc = NOTFOUND;
    } else if rc == NOTFOUND {
        rc = 0;
    }
    rc
}

/// `dumpit()`: the header and all records of one database. Returns C's
/// return code (0 on success).
fn dump_db(
    out: &mut Output,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    name: Option<&[u8]>,
    print: bool,
) -> i32 {
    match dump_db_inner(out, env, txn, dbi, name, print) {
        Ok(()) => 0,
        Err(rc) => rc,
    }
}

fn dump_db_inner(
    out: &mut Output,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    name: Option<&[u8]>,
    print: bool,
) -> Result<(), i32> {
    let code = |e: rlmdb::Error| err_code(&e);
    let flags = txn.db_flags(dbi).map_err(code)?;
    let st = txn.stat(dbi).map_err(code)?;
    let info = env.info().map_err(code)?;

    let mut header = Vec::with_capacity(256);
    header.extend_from_slice(b"VERSION=3\n");
    header.extend_from_slice(if print {
        b"format=print\n"
    } else {
        b"format=bytevalue\n"
    });
    if let Some(name) = name {
        header.extend_from_slice(b"database=");
        header.extend_from_slice(name);
        header.push(b'\n');
    }
    header.extend_from_slice(format!("type=btree\nmapsize={}\n", info.map_size).as_bytes());
    if info.map_address != 0 {
        header.extend_from_slice(
            format!("mapaddr={}\n", pointer(info.map_address as u64)).as_bytes(),
        );
    }
    header.extend_from_slice(format!("maxreaders={}\n", info.max_readers).as_bytes());
    if flags.contains(DatabaseFlags::DUP_SORT) {
        header.extend_from_slice(b"duplicates=1\n");
    }
    for (flag, flag_name) in DB_FLAG_NAMES {
        if flags.contains(flag) {
            header.extend_from_slice(format!("{flag_name}=1\n").as_bytes());
        }
    }
    header.extend_from_slice(format!("db_pagesize={}\nHEADER=END\n", st.page_size).as_bytes());
    out.write(&header);

    let mut cursor = txn.cursor(dbi).map_err(code)?;
    let mut line = Vec::new();
    let rc = loop {
        let (key, data) = match cursor.next() {
            Ok(Some(kv)) => kv,
            Ok(None) => break NOTFOUND,
            Err(e) => break err_code(&e),
        };
        // C checks its signal flag here; a closed pipe raised SIGPIPE.
        if let Some(e) = out.error() {
            let errno = io_errno(e);
            out.set_reported();
            break if errno == EPIPE { EINTR } else { errno };
        }
        line.clear();
        encode_line(&mut line, key, print);
        encode_line(&mut line, data, print);
        out.write(&line);
    };
    out.write(b"DATA=END\n");
    if rc == NOTFOUND {
        Ok(())
    } else {
        Err(rc)
    }
}
