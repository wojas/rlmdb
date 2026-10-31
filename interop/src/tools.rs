//! The upstream C tools (`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`) and
//! test programs (`mtest*`), run as subprocesses, with parsers for their
//! output. The helpers that take a [`TestEnv`] run the build of the
//! environment's release (`mdb_stat` for a 0.9 environment, `mdb_stat10`
//! for a 1.0 one); [`run_tool_any_status`] runs any program of [`bins`].
//!
//! The parsers document the exact output formats, which the Rust
//! reimplementations in `rlmdb-tools` must reproduce.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::peer::{default_timeout, run_command};
use crate::{bins, Error, Format, Output, Result, TestEnv};

/// The C tool `name` (`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`) of
/// the release that writes `format`.
fn tool_for(format: Format, name: &str) -> &'static Path {
    match (format, name) {
        (Format::V09, "mdb_stat") => bins::mdb_stat(),
        (Format::V09, "mdb_dump") => bins::mdb_dump(),
        (Format::V09, "mdb_load") => bins::mdb_load(),
        (Format::V09, "mdb_copy") => bins::mdb_copy(),
        (Format::V10, "mdb_stat") => bins::mdb_stat10(),
        (Format::V10, "mdb_dump") => bins::mdb_dump10(),
        (Format::V10, "mdb_load") => bins::mdb_load10(),
        (Format::V10, "mdb_copy") => bins::mdb_copy10(),
        _ => unreachable!("no C tool {name}"),
    }
}

/// Run a C program with arguments and optional stdin; require success.
fn run_tool(
    bin: &Path,
    args: &[OsString],
    cwd: Option<&Path>,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_tool_any_status(bin, args, cwd, stdin)?.ok()
}

/// As [`run_tool_any_status`], with environment variables set (`Some`) or
/// removed (`None`) for the program, such as `RLMDB_DEFAULT_FORMAT`.
pub fn run_tool_with_env(
    bin: &Path,
    args: &[OsString],
    envs: &[(&str, Option<&str>)],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    for (name, value) in envs {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
    let name = bin.file_name().unwrap_or_default().to_string_lossy();
    let label = std::iter::once(name.into_owned())
        .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    run_command(cmd, label, stdin, default_timeout())
}

/// The C LMDB 1.0 page audit (`cpeer10 check-freelist`) of the 1.0
/// environment at `env`: every page is in use exactly once (reachable from
/// the newest snapshot's trees or in the free list), and every page's
/// `mp_txnid` is plausible. `Err` with the audit's output if it fails.
pub fn audit10(env: &Path) -> Result<Output> {
    let args = [OsString::from("check-freelist"), env.as_os_str().to_owned()];
    run_tool_any_status(bins::cpeer10(), &args, None, None)?.ok()
}

/// Run a C program (`cwd`: its working directory) with arguments and
/// optional stdin, whatever its exit status; times out like the peers.
pub fn run_tool_any_status(
    bin: &Path,
    args: &[OsString],
    cwd: Option<&Path>,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let name = bin.file_name().unwrap_or_default().to_string_lossy();
    let label = std::iter::once(name.into_owned())
        .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    run_command(cmd, label, stdin, default_timeout())
}

fn tool_args<S: AsRef<OsStr>>(env: &TestEnv, flags: &[S]) -> Vec<OsString> {
    let mut args = env.tool_options();
    args.extend(flags.iter().map(|f| f.as_ref().to_owned()));
    args.push(env.path().into());
    args
}

/// Statistics of one B-tree as printed by `mdb_stat`:
///
/// ```text
///   Tree depth: 1
///   Branch pages: 0
///   Leaf pages: 1
///   Overflow pages: 0
///   Entries: 3
/// ```
///
/// LMDB 1.0's `mdb_stat` prints `  Page size: 4096` first (0.9 has that
/// line under `#if 0`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeStat {
    /// `Page size` (LMDB 1.0's `mdb_stat` only).
    pub page_size: Option<u32>,
    /// `Tree depth`.
    pub depth: u32,
    /// `Branch pages`.
    pub branch_pages: u64,
    /// `Leaf pages`.
    pub leaf_pages: u64,
    /// `Overflow pages`.
    pub overflow_pages: u64,
    /// `Entries`.
    pub entries: u64,
}

/// `mdb_stat -e`: the "Environment Info" section.
#[derive(Clone, Debug, Default)]
pub struct EnvInfo {
    /// `Map address` (`0x0` unless `MDB_FIXEDMAP`; printed with `%p`).
    pub map_address: String,
    /// `Map size`.
    pub map_size: u64,
    /// `Page size`.
    pub page_size: u32,
    /// `Max pages` (map size / page size).
    pub max_pages: u64,
    /// `Number of pages used` (last_pgno + 1).
    pub pages_used: u64,
    /// `Last transaction ID`.
    pub last_txnid: u64,
    /// `Max readers`.
    pub max_readers: u32,
    /// `Number of readers used` (slots ever used, `mti_numreaders`).
    pub readers_used: u32,
}

/// A line of the reader table (`mdb_reader_list`): `"%10d %zx %zu"` (pid,
/// thread id in hex, txnid) or `"%10d %zx -"` for a slot that is not in a
/// transaction, after the header `"    pid     thread     txnid"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderEntry {
    /// Process id.
    pub pid: i32,
    /// Thread id, hex digits as printed.
    pub thread: String,
    /// Snapshot txnid, `None` for `-`.
    pub txnid: Option<u64>,
}

/// `mdb_stat -f[f[f]]`: the "Freelist Status" section.
#[derive(Clone, Debug, Default)]
pub struct FreelistInfo {
    /// The free DB's statistics.
    pub stat: TreeStat,
    /// `Free pages` (sum of the free list lengths).
    pub free_pages: u64,
    /// With `-ff`: `(txnid, pages, maxspan)` per free DB record, from
    /// `"    Transaction %zu, %zd pages, maxspan %zd"`.
    pub transactions: Vec<(u64, u64, u64)>,
}

/// Parsed output of `mdb_stat`.
#[derive(Clone, Debug, Default)]
pub struct MdbStat {
    /// With `-e`.
    pub env: Option<EnvInfo>,
    /// With `-r`: the active readers (`"(no active readers)"` gives `[]`).
    pub readers: Option<Vec<ReaderEntry>>,
    /// With `-rr`: `"  %d stale readers cleared."`, the count from
    /// `mdb_reader_check`.
    pub stale_cleared: Option<u64>,
    /// With `-rr`: the reader table listed again after the check.
    pub readers_after_check: Option<Vec<ReaderEntry>>,
    /// With `-f`.
    pub freelist: Option<FreelistInfo>,
    /// `"Status of Main DB"` (name `None`), then with `-a` `"Status of <name>"`.
    pub dbs: Vec<(Option<String>, TreeStat)>,
}

impl MdbStat {
    /// Parse `mdb_stat` output.
    pub fn parse(text: &str) -> Result<MdbStat> {
        let mut out = MdbStat::default();
        let mut section = "";
        for line in text.lines() {
            if !line.starts_with(' ') && !line.starts_with('(') {
                section = line;
                if line == "Environment Info" {
                    out.env = Some(EnvInfo::default());
                } else if line == "Reader Table Status" {
                    out.readers = Some(Vec::new());
                } else if line == "Freelist Status" {
                    out.freelist = Some(FreelistInfo::default());
                } else if let Some(name) = line.strip_prefix("Status of ") {
                    let name = (name != "Main DB").then(|| name.to_owned());
                    out.dbs.push((name, TreeStat::default()));
                } else {
                    return Err(Error::new(format!("mdb_stat: unexpected line {line:?}")));
                }
                continue;
            }
            if section == "Reader Table Status" {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if let [n, "stale", "readers", "cleared."] = fields.as_slice() {
                    out.stale_cleared =
                        Some(n.parse().map_err(|_| {
                            Error::new(format!("mdb_stat: bad number in {line:?}"))
                        })?);
                    out.readers_after_check = Some(Vec::new());
                    continue;
                }
                let list = match out.readers_after_check.as_mut() {
                    Some(after) => after,
                    None => out.readers.as_mut().expect("section"),
                };
                parse_reader_line(line, list)?;
                continue;
            }
            if let Some(rest) = line.trim_start().strip_prefix("Transaction ") {
                // "Transaction 5, 3 pages, maxspan 2[ [bad sequence]]"
                let nums: Vec<u64> = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().unwrap_or(0))
                    .collect();
                if let (Some(fl), [t, p, s, ..]) = (out.freelist.as_mut(), nums.as_slice()) {
                    fl.transactions.push((*t, *p, *s));
                }
                continue;
            }
            let Some((key, value)) = line.trim().split_once(": ") else {
                continue; // -fff page lists
            };
            let num = |v: &str| -> Result<u64> {
                v.parse()
                    .map_err(|_| Error::new(format!("mdb_stat: bad number in {line:?}")))
            };
            let tree = match section {
                "Freelist Status" => out.freelist.as_mut().map(|f| &mut f.stat),
                s if s.starts_with("Status of ") => out.dbs.last_mut().map(|d| &mut d.1),
                _ => None,
            };
            if let Some(t) = tree {
                match key {
                    "Page size" => t.page_size = Some(num(value)? as u32),
                    "Tree depth" => t.depth = num(value)? as u32,
                    "Branch pages" => t.branch_pages = num(value)?,
                    "Leaf pages" => t.leaf_pages = num(value)?,
                    "Overflow pages" => t.overflow_pages = num(value)?,
                    "Entries" => t.entries = num(value)?,
                    "Free pages" => {
                        out.freelist.as_mut().expect("section").free_pages = num(value)?
                    }
                    _ => return Err(Error::new(format!("mdb_stat: unknown field {line:?}"))),
                }
                continue;
            }
            let e = out
                .env
                .as_mut()
                .ok_or_else(|| Error::new(format!("mdb_stat: stray line {line:?}")))?;
            match key {
                "Map address" => e.map_address = value.to_owned(),
                "Map size" => e.map_size = num(value)?,
                "Page size" => e.page_size = num(value)? as u32,
                "Max pages" => e.max_pages = num(value)?,
                "Number of pages used" => e.pages_used = num(value)?,
                "Last transaction ID" => e.last_txnid = num(value)?,
                "Max readers" => e.max_readers = num(value)? as u32,
                "Number of readers used" => e.readers_used = num(value)? as u32,
                _ => return Err(Error::new(format!("mdb_stat: unknown field {line:?}"))),
            }
        }
        Ok(out)
    }

    /// The statistics of DB `name` (`None` = main DB).
    pub fn db(&self, name: Option<&str>) -> Option<&TreeStat> {
        self.dbs
            .iter()
            .find(|(n, _)| n.as_deref() == name)
            .map(|(_, s)| s)
    }
}

fn parse_reader_line(line: &str, readers: &mut Vec<ReaderEntry>) -> Result<()> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    match fields.as_slice() {
        ["pid", "thread", "txnid"] | ["(no", "active", "readers)"] => Ok(()),
        [pid, thread, txnid] => {
            readers.push(ReaderEntry {
                pid: pid
                    .parse()
                    .map_err(|_| Error::new(format!("bad reader line {line:?}")))?,
                thread: (*thread).to_owned(),
                txnid: if *txnid == "-" {
                    None
                } else {
                    Some(
                        txnid
                            .parse()
                            .map_err(|_| Error::new(format!("bad reader line {line:?}")))?,
                    )
                },
            });
            Ok(())
        }
        _ => Err(Error::new(format!("unexpected reader table line {line:?}"))),
    }
}

/// Run `mdb_stat <flags> <env>` (`mdb_stat10` for a 1.0 environment) and
/// parse the output.
///
/// Quirk: `mdb_stat -r` without `-a`, `-s` or `-f` exits with status 1 on
/// success, because it returns the last `fputs()` result of
/// `mdb_reader_list()` (a positive number) as its error code. That case is
/// accepted here when nothing was printed to stderr.
pub fn mdb_stat(env: &TestEnv, flags: &[&str]) -> Result<MdbStat> {
    let out = run_tool_any_status(
        tool_for(env.format(), "mdb_stat"),
        &tool_args(env, flags),
        None,
        None,
    )?;
    let readers_only = flags.iter().any(|f| f.starts_with("-r"))
        && !flags
            .iter()
            .any(|f| f.starts_with("-a") || f.starts_with("-s") || f.starts_with("-f"));
    let out = if readers_only && out.code() == Some(1) && out.stderr.is_empty() {
        out
    } else {
        out.ok()?
    };
    MdbStat::parse(&out.stdout).map_err(|e| e.context(format!("parsing:\n{}", out.stdout)))
}

/// One database of an `mdb_dump` output: header lines, then data lines.
///
/// ```text
/// VERSION=3
/// format=bytevalue
/// database=<name>        (only for named DBs)
/// type=btree
/// mapsize=<n>
/// maxreaders=<n>
/// duplicates=1           (DUPSORT; also dupsort=1 etc. per DB flag)
/// db_pagesize=<n>
/// HEADER=END
///  <hex key>
///  <hex value>
/// DATA=END
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DumpedDb {
    /// `database=`, `None` for the main DB.
    pub name: Option<String>,
    /// All header `key=value` pairs, in order.
    pub header: Vec<(String, String)>,
    /// Key/value pairs (hex-decoded), in cursor order.
    pub records: Vec<(Vec<u8>, Vec<u8>)>,
}

impl DumpedDb {
    /// A header value.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(Error::new(format!("odd hex length: {s}")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| Error::new(format!("bad hex: {s}")))
        })
        .collect()
}

/// Parse `mdb_dump` output (byte-value format, one or more DBs).
pub fn parse_dump(text: &str) -> Result<Vec<DumpedDb>> {
    let mut dbs = Vec::new();
    let mut lines = text.lines();
    while let Some(first) = lines.next() {
        if first != "VERSION=3" {
            return Err(Error::new(format!(
                "mdb_dump: expected VERSION=3, got {first:?}"
            )));
        }
        let mut db = DumpedDb::default();
        for line in lines.by_ref() {
            if line == "HEADER=END" {
                break;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| Error::new(format!("mdb_dump: bad header line {line:?}")))?;
            if k == "database" {
                db.name = Some(v.to_owned());
            }
            db.header.push((k.to_owned(), v.to_owned()));
        }
        loop {
            let key = lines
                .next()
                .ok_or_else(|| Error::new("mdb_dump: missing DATA=END"))?;
            if key == "DATA=END" {
                break;
            }
            let value = lines
                .next()
                .ok_or_else(|| Error::new("mdb_dump: key without value"))?;
            let strip = |l: &str| l.strip_prefix(' ').map(str::to_owned);
            let (Some(k), Some(v)) = (strip(key), strip(value)) else {
                return Err(Error::new(format!(
                    "mdb_dump: bad data lines {key:?} {value:?}"
                )));
            };
            db.records.push((unhex(&k)?, unhex(&v)?));
        }
        dbs.push(db);
    }
    Ok(dbs)
}

/// Run `mdb_dump <flags> <env>` (`mdb_dump10` for a 1.0 environment);
/// returns the raw text.
pub fn mdb_dump_text(env: &TestEnv, flags: &[&str]) -> Result<String> {
    let bin = tool_for(env.format(), "mdb_dump");
    Ok(run_tool(bin, &tool_args(env, flags), None, None)?.stdout)
}

/// Run `mdb_dump <flags> <env>` (`mdb_dump10` for a 1.0 environment) and
/// parse the output.
pub fn mdb_dump(env: &TestEnv, flags: &[&str]) -> Result<Vec<DumpedDb>> {
    parse_dump(&mdb_dump_text(env, flags)?)
}

/// Run `mdb_load <flags> <env>` (`mdb_load10` for a 1.0 environment) with
/// `input` on stdin. For a directory environment, the directory is created
/// first.
pub fn mdb_load(env: &TestEnv, flags: &[&str], input: &str) -> Result<Output> {
    if !env.is_nosubdir() {
        std::fs::create_dir_all(env.path())?;
    }
    run_tool(
        tool_for(env.format(), "mdb_load"),
        &tool_args(env, flags),
        None,
        Some(input.as_bytes()),
    )
}

/// Run `mdb_copy [-c] <src> <dst>` (`mdb_copy10` for a 1.0 environment).
/// For a directory environment, `dst` is created first (mdb_copy requires
/// it to exist; the data file inside must not).
pub fn mdb_copy(src: &TestEnv, dst: &TestEnv, compact: bool) -> Result<Output> {
    mdb_copy_any_status(src, dst, compact)?.ok()
}

/// [`mdb_copy`], whatever its exit status.
pub fn mdb_copy_any_status(src: &TestEnv, dst: &TestEnv, compact: bool) -> Result<Output> {
    if !dst.is_nosubdir() {
        std::fs::create_dir_all(dst.path())?;
    }
    let mut args = src.tool_options();
    if compact {
        args.push("-c".into());
    }
    args.push(src.path().into());
    args.push(dst.path().into());
    run_tool_any_status(tool_for(src.format(), "mdb_copy"), &args, None, None)
}

/// The output of `layout_dump`: sizes, offsets and constants of the C build.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// `lock_mode=`: `posix_sem` (macOS, BSD), `posix_mutex` (Linux) or
    /// `win32` (Windows: named mutexes).
    pub lock_mode: String,
    /// Every numeric `name=value` line, e.g. `sizeof(MDB_meta)` or
    /// `offsetof(MDB_txninfo.mti_readers)`.
    pub values: std::collections::BTreeMap<String, u64>,
}

impl Layout {
    /// Parse `layout_dump` output.
    pub fn parse(text: &str) -> Result<Layout> {
        let mut l = Layout::default();
        for line in text.lines() {
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| Error::new(format!("layout_dump: bad line {line:?}")))?;
            if k == "lock_mode" {
                l.lock_mode = v.to_owned();
                continue;
            }
            let n = match v.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => v.parse(),
            }
            .map_err(|_| Error::new(format!("layout_dump: bad value {line:?}")))?;
            l.values.insert(k.to_owned(), n);
        }
        Ok(l)
    }

    /// A value by name.
    pub fn get(&self, name: &str) -> Result<u64> {
        self.values
            .get(name)
            .copied()
            .ok_or_else(|| Error::new(format!("layout_dump has no {name}")))
    }
}

/// Run `layout_dump` (once per process) and parse it.
pub fn c_layout() -> Result<&'static Layout> {
    static LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();
    if let Some(l) = LAYOUT.get() {
        return Ok(l);
    }
    let out = run_tool(bins::layout_dump(), &[], None, None)?;
    let l = Layout::parse(&out.stdout)?;
    Ok(LAYOUT.get_or_init(|| l))
}

/// Run `layout_dump10`, the LMDB 1.0 build of `layout_dump` (once per
/// process), and parse it.
pub fn c_layout10() -> Result<&'static Layout> {
    static LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();
    if let Some(l) = LAYOUT.get() {
        return Ok(l);
    }
    let out = run_tool(bins::layout_dump10(), &[], None, None)?;
    let l = Layout::parse(&out.stdout)?;
    Ok(LAYOUT.get_or_init(|| l))
}

/// `layout_dump` of the release that writes `format`: [`c_layout`] or
/// [`c_layout10`].
pub fn c_layout_for(format: Format) -> Result<&'static Layout> {
    match format {
        Format::V09 => c_layout(),
        Format::V10 => c_layout10(),
    }
}

/// Run the upstream test program `name` (`mtest`, `mtest2`, …) in `dir`;
/// it creates/uses `dir/testdb`.
pub fn run_mtest(name: &str, dir: &Path) -> Result<Output> {
    run_mtest_in(Format::V09, name, dir)
}

/// [`run_mtest`] with the build of the release that writes `format`:
/// `mtest*10` for 1.0 ([`bins::mtest10`], 1 KiB pages).
pub fn run_mtest_in(format: Format, name: &str, dir: &Path) -> Result<Output> {
    let bin = match format {
        Format::V09 => bins::mtest(name),
        Format::V10 => bins::mtest10(name),
    }
    .ok_or_else(|| Error::new(format!("unknown mtest {name}")))?;
    run_tool(bin, &[], Some(dir), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mdb_stat() {
        let text = "Environment Info\n  Map address: 0x0\n  Map size: 1048576\n  Page size: 16384\n  Max pages: 64\n  Number of pages used: 5\n  Last transaction ID: 3\n  Max readers: 126\n  Number of readers used: 1\nReader Table Status\n    pid     thread     txnid\n     12345 16f3a7000 3\n     12346 16f3a8000 -\nFreelist Status\n  Tree depth: 1\n  Branch pages: 0\n  Leaf pages: 1\n  Overflow pages: 0\n  Entries: 2\n    Transaction 2, 1 pages, maxspan 1\n    Transaction 3, 2 pages, maxspan 2\n  Free pages: 3\nStatus of Main DB\n  Tree depth: 1\n  Branch pages: 0\n  Leaf pages: 1\n  Overflow pages: 0\n  Entries: 1\nStatus of sub\n  Tree depth: 2\n  Branch pages: 1\n  Leaf pages: 3\n  Overflow pages: 0\n  Entries: 300\n";
        let s = MdbStat::parse(text).unwrap();
        let e = s.env.as_ref().unwrap();
        assert_eq!(e.page_size, 16384);
        assert_eq!(e.pages_used, 5);
        let r = s.readers.as_ref().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].pid, 12345);
        assert_eq!(r[1].txnid, None);
        let f = s.freelist.as_ref().unwrap();
        assert_eq!(f.free_pages, 3);
        assert_eq!(f.transactions, vec![(2, 1, 1), (3, 2, 2)]);
        assert_eq!(s.dbs.len(), 2);
        assert_eq!(s.db(Some("sub")).unwrap().entries, 300);
        assert_eq!(s.stale_cleared, None);
    }

    #[test]
    fn parses_mdb_stat_rr() {
        let text = "Reader Table Status\n    pid     thread     txnid\n     12345 16f3a7000 3\n  1 stale readers cleared.\n(no active readers)\n";
        let s = MdbStat::parse(text).unwrap();
        assert_eq!(s.readers.as_ref().unwrap().len(), 1);
        assert_eq!(s.stale_cleared, Some(1));
        assert_eq!(s.readers_after_check, Some(Vec::new()));
    }

    #[test]
    fn parses_mdb_dump() {
        let text = "VERSION=3\nformat=bytevalue\ndatabase=x\ntype=btree\nmapsize=1048576\nmaxreaders=126\nduplicates=1\ndupsort=1\ndb_pagesize=4096\nHEADER=END\n 6b31\n 7631\nDATA=END\n";
        let d = parse_dump(text).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name.as_deref(), Some("x"));
        assert_eq!(d[0].header("dupsort"), Some("1"));
        assert_eq!(d[0].records, vec![(b"k1".to_vec(), b"v1".to_vec())]);
    }
}
