//! `mdb_drop`: empty or delete a database.
//!
//! ```text
//! mdb_drop [-V] [-n] [-d] [-s subdb] dbpath
//! ```
//!
//! LMDB 0.9.36 has no `mdb_drop`; this one follows LMDB's own `mdb_drop.c`
//! as first added (it later gained `-L` and the 1.0 crypto options `-m`/`-w`,
//! which have no 0.9 counterpart): by default the database (`-s subdb`, or
//! the main DB) is emptied (`mdb_drop(txn, dbi, 0)`); with `-d` a named
//! database is deleted from the environment (`mdb_drop(txn, dbi, 1)`; for
//! the main DB this also just empties it). Everything happens in one write
//! transaction.

use lmdb_core::{DatabaseFlags, Environment, EnvironmentFlags};

use crate::common::{create_db, path_of, report_failed, Tool, EXIT_FAILURE};
use crate::getopt::{GetOpt, Opt};
use crate::output::eprint_bytes;

const SYNOPSIS: &str = "[-V] [-n] [-d] [-s subdb] dbpath";

/// The `main` of `mdb_drop`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    if args.len() < 2 {
        return tool.usage(SYNOPSIS);
    }
    let mut delete = false;
    let mut no_sub_dir = false;
    let mut subname: Option<Vec<u8>> = None;
    let mut getopt = GetOpt::new(args, "dns:V");
    while let Some(opt) = getopt.next() {
        match opt {
            Opt::Flag(b'V') => return tool.version(),
            Opt::Flag(b'd') => delete = true,
            Opt::Flag(b'n') => no_sub_dir = true,
            Opt::Arg(b's', name) => subname = Some(name),
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

    let mut builder = Environment::builder();
    builder.max_dbs(2);
    if no_sub_dir {
        builder.flags(EnvironmentFlags::NO_SUB_DIR);
    }
    let env = match builder.open_with_permissions(path_of(&operands[0]), 0o664) {
        Ok(env) => env,
        Err(e) => {
            report_failed("mdb_env_open", &e);
            return EXIT_FAILURE;
        }
    };
    let mut txn = match env.begin_rw_txn() {
        Ok(t) => t,
        Err(e) => {
            report_failed("mdb_txn_begin", &e);
            return EXIT_FAILURE;
        }
    };
    // mdb_open(txn, subname, 0, &dbi): an existing database only.
    let dbi = match create_db(&mut txn, subname.as_deref(), DatabaseFlags::empty()) {
        Ok(d) => d,
        Err(e) => {
            report_failed("mdb_open", &e);
            return EXIT_FAILURE;
        }
    };
    let dropped = if delete {
        txn.drop_db(dbi)
    } else {
        txn.clear_db(dbi)
    };
    if let Err(e) = dropped {
        report_failed("mdb_drop", &e);
        return EXIT_FAILURE;
    }
    if let Err(e) = txn.commit() {
        report_failed("mdb_txn_commit", &e);
        return EXIT_FAILURE;
    }
    0
}
