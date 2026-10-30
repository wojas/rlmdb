//! `mdb_stat`: environment, free list, reader table and database
//! statistics (`libraries/liblmdb/mdb_stat.c`).
//!
//! ```text
//! mdb_stat [-V] [-n] [-e] [-r[r]] [-f[f[f]]] [-a|-s subdb] dbpath
//! ```
//!
//! Output and messages are those of the C tool of the environment's format:
//! for an LMDB 1.0 environment those of 1.0's `mdb_stat` (a `Page size` line
//! in every database's block, `mdb_dbi_open` in the message, `-a` lists the
//! records of named databases, whose keys end with a NUL). One deliberate
//! difference (`KNOWN_C_BUGS.md` #2): `mdb_stat -r` without `-a`, `-s` or
//! `-f` exits with 0 on success; C exits with 1 because it returns the
//! value of the last `fputs` call.

use rlmdb::{format_reader_list, Dbi, Environment, EnvironmentFlags, Format, Transaction};

use crate::cfmt::{err_code, pointer};
use crate::common::{
    db_name_of, dbi_open_name, open_db, path_of, print_db_stat, report_failed, Tool, EXIT_FAILURE,
};
use crate::getopt::{GetOpt, Opt};
use crate::output::{eprint_bytes, Output};

const SYNOPSIS: &str = "[-V] [-n] [-e] [-r[r]] [-f[f[f]]] [-a|-s subdb] dbpath";

/// `MDB_NOTFOUND`.
const NOTFOUND: i32 = rlmdb::consts::MDB_NOTFOUND;

/// Command line options.
#[derive(Debug, Default)]
struct Options {
    /// `-a` (counted, as in C).
    all_dbs: u32,
    /// `-e`.
    env_info: u32,
    /// `-f`, `-ff`, `-fff`.
    free_info: u32,
    /// `-r`, `-rr`.
    reader_info: u32,
    /// `-n`.
    no_sub_dir: bool,
    /// `-s subdb`.
    subname: Option<Vec<u8>>,
}

/// The `main` of `mdb_stat`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    if args.len() < 2 {
        return tool.usage(SYNOPSIS);
    }
    let mut o = Options::default();
    let mut getopt = GetOpt::new(args, "Vaefnrs:");
    while let Some(opt) = getopt.next() {
        match opt {
            Opt::Flag(b'V') => return tool.version(),
            Opt::Flag(b'a') => {
                if o.subname.is_some() {
                    return tool.usage(SYNOPSIS);
                }
                o.all_dbs += 1;
            }
            Opt::Flag(b'e') => o.env_info += 1,
            Opt::Flag(b'f') => o.free_info += 1,
            Opt::Flag(b'n') => o.no_sub_dir = true,
            Opt::Flag(b'r') => o.reader_info += 1,
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
        builder.max_dbs(4);
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
    let rc = stat_env(tool, &env, &o);
    if rc != 0 {
        EXIT_FAILURE
    } else {
        0
    }
}

/// Everything after `mdb_env_open`. Returns C's final `rc`.
fn stat_env(tool: &mut Tool, env: &Environment, o: &Options) -> i32 {
    let out = &mut tool.out;
    let format = env.format();
    if o.env_info > 0 {
        if let Err(rc) = print_env_info(out, env) {
            return rc;
        }
    }

    if o.reader_info > 0 {
        out.write(b"Reader Table Status\n");
        print_reader_table(out, env);
        if o.reader_info > 1 {
            let dead = env.reader_check().unwrap_or(0);
            out.print(format_args!("  {dead} stale readers cleared.\n"));
            print_reader_table(out, env);
        }
        if !(o.subname.is_some() || o.all_dbs > 0 || o.free_info > 0) {
            // C returns the result of the last fputs here, i.e. exits with 1
            // (KNOWN_C_BUGS.md #2). Success is 0.
            return 0;
        }
    }

    let txn = match env.begin_ro_txn() {
        Ok(t) => t,
        Err(e) => {
            report_failed("mdb_txn_begin", &e);
            return err_code(&e);
        }
    };

    if o.free_info > 0 {
        if let Err(rc) = print_freelist(out, format, &txn, o.free_info) {
            return rc;
        }
    }

    let dbi = match open_db(&txn, o.subname.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            report_failed(dbi_open_name(env), &e);
            return err_code(&e);
        }
    };
    let st = match txn.stat(dbi) {
        Ok(s) => s,
        Err(e) => {
            report_failed("mdb_stat", &e);
            return err_code(&e);
        }
    };
    out.write(b"Status of ");
    out.write(o.subname.as_deref().unwrap_or(b"Main DB"));
    out.write(b"\n");
    print_db_stat(out, format, &st);

    let mut rc = 0;
    if o.all_dbs > 0 {
        let mut cursor = match txn.cursor(dbi) {
            Ok(c) => c,
            Err(e) => {
                report_failed("mdb_cursor_open", &e);
                return err_code(&e);
            }
        };
        rc = loop {
            let key = match cursor.next_nodup() {
                Ok(Some((key, _))) => key,
                Ok(None) => break NOTFOUND,
                Err(e) => break err_code(&e),
            };
            let Some(name) = db_name_of(format, &cursor, key) else {
                continue;
            };
            let Ok(db2) = open_db(&txn, Some(name)) else {
                continue;
            };
            out.write(b"Status of ");
            out.write(name);
            out.write(b"\n");
            match txn.stat(db2) {
                Ok(st) => print_db_stat(out, format, &st),
                Err(e) => {
                    report_failed("mdb_stat", &e);
                    return err_code(&e);
                }
            }
            env.close_db(db2);
        };
    }
    if rc == NOTFOUND {
        rc = 0;
    }
    rc
}

/// `-e`: the "Environment Info" block (`mdb_env_stat` + `mdb_env_info`).
fn print_env_info(out: &mut Output, env: &Environment) -> Result<(), i32> {
    let st = env.stat().map_err(|e| {
        report_failed("mdb_env_stat", &e);
        err_code(&e)
    })?;
    let info = env.info().map_err(|e| {
        report_failed("mdb_env_info", &e);
        err_code(&e)
    })?;
    let page_size = st.page_size.max(1) as usize;
    out.print(format_args!(
        "Environment Info\n  Map address: {}\n  Map size: {}\n  Page size: {}\n  Max pages: {}\n  \
         Number of pages used: {}\n  Last transaction ID: {}\n  Max readers: {}\n  \
         Number of readers used: {}\n",
        pointer(info.map_address as u64),
        info.map_size,
        st.page_size,
        info.map_size / page_size,
        info.last_pgno.wrapping_add(1),
        info.last_txnid,
        info.max_readers,
        info.num_readers,
    ));
    Ok(())
}

/// `mdb_reader_list(env, fputs, stdout)`.
fn print_reader_table(out: &mut Output, env: &Environment) {
    if !env.has_lock_file() {
        out.write(b"(no reader locks)\n");
        return;
    }
    // C prints nothing if the list cannot be read (it cannot fail in C).
    if let Ok(readers) = env.reader_list() {
        out.write(format_reader_list(&readers).as_bytes());
    }
}

/// `-f`: the "Freelist Status" block; `-ff` adds a summary line per free
/// list record, `-fff` its page numbers.
fn print_freelist<T: Transaction>(
    out: &mut Output,
    format: Format,
    txn: &T,
    level: u32,
) -> Result<(), i32> {
    out.write(b"Freelist Status\n");
    let mut cursor = txn.cursor(Dbi::FREE).map_err(|e| {
        report_failed("mdb_cursor_open", &e);
        err_code(&e)
    })?;
    let st = txn.stat(Dbi::FREE).map_err(|e| {
        report_failed("mdb_stat", &e);
        err_code(&e)
    })?;
    print_db_stat(out, format, &st);
    let mut pages = 0u64;
    // As in C, an error while walking the free DB just ends the walk.
    while let Ok(Some((key, data))) = cursor.next() {
        let record = FreeRecord::parse(key, data);
        pages = pages.wrapping_add(record.count);
        if level > 1 {
            record.print_summary(out);
            if level > 2 {
                record.print_pages(out);
            }
        }
    }
    out.print(format_args!("  Free pages: {pages}\n"));
    Ok(())
}

/// A record of the free DB: the transaction that freed the pages and the
/// page numbers (an `MDB_IDL`: count, then the ids in descending order).
struct FreeRecord {
    txnid: u64,
    /// The stored count (C trusts it; we only read the ids that exist).
    count: u64,
    ids: Vec<u64>,
}

impl FreeRecord {
    fn parse(key: &[u8], data: &[u8]) -> FreeRecord {
        let word = |b: &[u8]| {
            let mut w = [0u8; 8];
            let n = b.len().min(8);
            w[..n].copy_from_slice(&b[..n]);
            u64::from_ne_bytes(w)
        };
        let count = word(data);
        let ids = data
            .get(8..)
            .unwrap_or(&[])
            .chunks_exact(8)
            .map(word)
            .take(usize::try_from(count).unwrap_or(usize::MAX))
            .collect();
        FreeRecord {
            txnid: word(key),
            count,
            ids,
        }
    }

    /// `-ff`: `"    Transaction T, N pages, maxspan S"`, flagging page
    /// numbers that are not strictly descending or include a meta page.
    fn print_summary(&self, out: &mut Output) {
        let ids = &self.ids;
        let mut bad = "";
        let mut span = 0usize;
        let mut prev = 1u64;
        // Walk from the end (smallest page) upwards, as mdb_stat.c does,
        // tracking the longest run of consecutive pages.
        for i in (0..ids.len()).rev() {
            let mut pg = ids[i];
            if pg <= prev {
                bad = " [bad sequence]";
            }
            prev = pg;
            pg = pg.wrapping_add(span as u64);
            while i >= span && ids[i - span] == pg {
                span += 1;
                pg = pg.wrapping_add(1);
            }
        }
        out.print(format_args!(
            "    Transaction {}, {} pages, maxspan {}{}\n",
            self.txnid, self.count as i64, span, bad
        ));
    }

    /// `-fff`: the page numbers, runs of consecutive pages as `first[len]`.
    fn print_pages(&self, out: &mut Output) {
        let ids = &self.ids;
        let mut j = ids.len();
        while j > 0 {
            j -= 1;
            let pg = ids[j];
            let mut span = 1u64;
            while j > 0 && ids[j - 1] == pg.wrapping_add(span) {
                j -= 1;
                span += 1;
            }
            if span > 1 {
                out.print(format_args!("     {pg:9}[{span}]\n"));
            } else {
                out.print(format_args!("     {pg:9}\n"));
            }
        }
    }
}
