//! Parsers for the output of peer commands.
//!
//! Peers print `key=value` tokens separated by spaces, one record per line
//! (`README.md` lists every command's lines). Both peers must print the same
//! format, so these parsers double as its specification.

use std::str::FromStr;

use crate::{Error, Result};

/// The `key=value` tokens of one line.
pub fn tokens(line: &str) -> impl Iterator<Item = (&str, &str)> {
    line.split_whitespace().filter_map(|t| t.split_once('='))
}

/// All `key=value` tokens of a text, in order.
#[derive(Clone, Debug, Default)]
pub struct Kv {
    pairs: Vec<(String, String)>,
}

impl Kv {
    /// Collect the tokens of every line of `text`.
    pub fn parse(text: &str) -> Kv {
        Kv::from_lines(text.lines())
    }

    /// Collect the tokens of the given lines.
    pub fn from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Kv {
        let pairs = lines
            .into_iter()
            .flat_map(tokens)
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        Kv { pairs }
    }

    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The first value of `key`, parsed.
    pub fn num<T: FromStr>(&self, key: &str) -> Result<T> {
        let v = self
            .get(key)
            .ok_or_else(|| Error::new(format!("no `{key}=` in output")))?;
        parse_num(v).ok_or_else(|| Error::new(format!("bad number `{key}={v}`")))
    }
}

/// Parse a decimal or `0x` hex number.
fn parse_num<T: FromStr>(v: &str) -> Option<T> {
    if let Some(hex) = v.strip_prefix("0x") {
        let n = u64::from_str_radix(hex, 16).ok()?;
        n.to_string().parse().ok()
    } else {
        v.parse().ok()
    }
}

/// One B-tree, as printed by `stat` and `check-freelist`:
/// `db=<name> [flags=0x..] depth=.. branch_pages=.. leaf_pages=..
/// overflow_pages=.. entries=..`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeLine {
    /// `-` for the main DB, `@free` for the free DB, else the DB name.
    pub name: String,
    /// The DB flags (`MDB_DUPSORT` …), if printed.
    pub flags: Option<u32>,
    /// Tree depth.
    pub depth: u32,
    /// Branch pages (for DUPSORT DBs: including sub-DB pages).
    pub branch_pages: u64,
    /// Leaf pages (for DUPSORT DBs: including sub-DB pages).
    pub leaf_pages: u64,
    /// Overflow pages.
    pub overflow_pages: u64,
    /// Data items (for DUPSORT DBs: every value counts).
    pub entries: u64,
    /// `check-freelist` only: DUPSORT keys whose values are in a sub-DB.
    pub subdbs: Option<u64>,
    /// `check-freelist` only: DUPSORT keys whose values are in a sub-page.
    pub subpages: Option<u64>,
}

impl TreeLine {
    /// Parse a `db=...` line.
    pub fn parse(line: &str) -> Result<TreeLine> {
        let kv = Kv::from_lines([line]);
        Ok(TreeLine {
            name: kv
                .get("db")
                .ok_or_else(|| Error::new(format!("not a db line: {line}")))?
                .to_owned(),
            flags: kv.get("flags").and_then(parse_num),
            depth: kv.num("depth")?,
            branch_pages: kv.num("branch_pages")?,
            leaf_pages: kv.num("leaf_pages")?,
            overflow_pages: kv.num("overflow_pages")?,
            entries: kv.num("entries")?,
            subdbs: kv.get("subdbs").and_then(parse_num),
            subpages: kv.get("subpages").and_then(parse_num),
        })
    }

    /// All pages of the tree.
    pub fn pages(&self) -> u64 {
        self.branch_pages + self.leaf_pages + self.overflow_pages
    }
}

fn tree_lines(text: &str) -> Result<Vec<TreeLine>> {
    text.lines()
        .filter(|l| l.starts_with("db="))
        .map(TreeLine::parse)
        .collect()
}

fn find_tree<'a>(dbs: &'a [TreeLine], name: &str) -> Result<&'a TreeLine> {
    dbs.iter()
        .find(|d| d.name == name)
        .ok_or_else(|| Error::new(format!("no line for DB {name}")))
}

/// Output of `create`.
#[derive(Clone, Debug)]
pub struct CreateReport {
    /// Page size of the new environment (the OS page size for C).
    pub psize: u32,
    /// Map size.
    pub mapsize: u64,
    /// `mdb_env_get_maxkeysize`.
    pub maxkeysize: u32,
    /// Last committed transaction (0 for a new environment).
    pub last_txnid: u64,
}

impl CreateReport {
    /// Parse the `created ...` line.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::parse(text);
        Ok(CreateReport {
            psize: kv.num("psize")?,
            mapsize: kv.num("mapsize")?,
            maxkeysize: kv.num("maxkeysize")?,
            last_txnid: kv.num("last_txnid")?,
        })
    }
}

/// Output of `fill` (`filled ...`) or `delete` (`deleted ...`).
#[derive(Clone, Debug)]
pub struct WriteReport {
    /// Keys written or deleted.
    pub keys: u64,
    /// For fill: entries in the DB afterwards; for delete: entries deleted.
    pub entries: u64,
    /// Transactions committed.
    pub txns: u64,
    /// The rest of the line (fill prints DB statistics).
    pub kv: Kv,
}

impl WriteReport {
    /// Parse the `filled`/`deleted` line.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::parse(text);
        Ok(WriteReport {
            keys: kv.num("keys")?,
            entries: kv.num("entries")?,
            txns: kv.num("txns")?,
            kv,
        })
    }
}

/// Output of `verify`: `verified db=.. keys=.. entries=.. absent=..`.
#[derive(Clone, Debug)]
pub struct VerifyReport {
    /// Keys present.
    pub keys: u64,
    /// Key/value pairs present.
    pub entries: u64,
    /// Keys checked to be absent (deleted).
    pub absent: u64,
}

impl VerifyReport {
    /// Parse the `verified` line.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::parse(text);
        Ok(VerifyReport {
            keys: kv.num("keys")?,
            entries: kv.num("entries")?,
            absent: kv.num("absent")?,
        })
    }
}

/// Output of `stat`.
#[derive(Clone, Debug)]
pub struct StatReport {
    /// Page size.
    pub psize: u32,
    /// Map size of the reading process.
    pub mapsize: u64,
    /// Last used page.
    pub last_pgno: u64,
    /// Last committed transaction.
    pub last_txnid: u64,
    /// Reader table size.
    pub maxreaders: u32,
    /// Reader slots ever used (`mti_numreaders`).
    pub numreaders: u32,
    /// `mdb_env_get_maxkeysize`.
    pub maxkeysize: u32,
    /// Pages listed in the free DB.
    pub free_pages: u64,
    /// `@free`, `-`, then every named DB.
    pub dbs: Vec<TreeLine>,
}

impl StatReport {
    /// Parse the output of `stat`.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::from_lines(text.lines().filter(|l| !l.starts_with("db=")));
        Ok(StatReport {
            psize: kv.num("psize")?,
            mapsize: kv.num("mapsize")?,
            last_pgno: kv.num("last_pgno")?,
            last_txnid: kv.num("last_txnid")?,
            maxreaders: kv.num("maxreaders")?,
            numreaders: kv.num("numreaders")?,
            maxkeysize: kv.num("maxkeysize")?,
            free_pages: kv.num("free_pages")?,
            dbs: tree_lines(text)?,
        })
    }

    /// The line of DB `name` (`-` = main, `@free` = free DB).
    pub fn db(&self, name: &str) -> Result<&TreeLine> {
        find_tree(&self.dbs, name)
    }
}

/// Output of `check-freelist`.
#[derive(Clone, Debug)]
pub struct FreelistReport {
    /// Page size.
    pub psize: u32,
    /// The audited snapshot.
    pub txnid: u64,
    /// Last page of the snapshot.
    pub last_pgno: u64,
    /// Every tree, as found by the walk.
    pub dbs: Vec<TreeLine>,
    /// Pages of the free DB's own tree.
    pub pages_freedb: u64,
    /// Pages listed as free in the free DB.
    pub pages_free: u64,
    /// Pages of all other trees.
    pub pages_tree: u64,
    /// Pages used by nothing (must be 0).
    pub pages_leaked: u64,
    /// `last_pgno + 1`.
    pub pages_total: u64,
}

impl FreelistReport {
    /// Parse the output of a *successful* `check-freelist` (ending in `ok`).
    pub fn parse(text: &str) -> Result<Self> {
        if text.lines().last() != Some("ok") {
            return Err(Error::new(format!(
                "check-freelist did not end with `ok`:\n{text}"
            )));
        }
        let kv = Kv::from_lines(text.lines().filter(|l| !l.starts_with("db=")));
        Ok(FreelistReport {
            psize: kv.num("psize")?,
            txnid: kv.num("txnid")?,
            last_pgno: kv.num("last_pgno")?,
            dbs: tree_lines(text)?,
            pages_freedb: kv.num("pages_freedb")?,
            pages_free: kv.num("pages_free")?,
            pages_tree: kv.num("pages_tree")?,
            pages_leaked: kv.num("pages_leaked")?,
            pages_total: kv.num("pages_total")?,
        })
    }

    /// The line of DB `name`.
    pub fn db(&self, name: &str) -> Result<&TreeLine> {
        find_tree(&self.dbs, name)
    }
}

/// One slot of the reader table as printed by `readers`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderSlot {
    /// Owning process.
    pub pid: i32,
    /// Owning thread, as printed (`0x...`).
    pub tid: String,
    /// Snapshot, or `None` if the slot is not in a transaction (`-`).
    pub txnid: Option<u64>,
}

/// Output of `readers`.
#[derive(Clone, Debug)]
pub struct ReadersReport {
    /// Slots with a pid, in table order.
    pub readers: Vec<ReaderSlot>,
    /// Slots ever used (`mti_numreaders`).
    pub numreaders: u32,
    /// Table size.
    pub maxreaders: u32,
}

impl ReadersReport {
    /// Parse the output of `readers`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut readers = Vec::new();
        for line in text.lines().filter(|l| l.starts_with("reader ")) {
            let kv = Kv::from_lines([line]);
            let txnid = match kv.get("txnid") {
                Some("-") => None,
                _ => Some(kv.num("txnid")?),
            };
            readers.push(ReaderSlot {
                pid: kv.num("pid")?,
                tid: kv.get("tid").unwrap_or_default().to_owned(),
                txnid,
            });
        }
        let kv = Kv::from_lines(text.lines().filter(|l| l.starts_with("readers=")));
        let n: usize = kv.num("readers")?;
        if n != readers.len() {
            return Err(Error::new(format!(
                "readers={n} but {} reader lines",
                readers.len()
            )));
        }
        Ok(ReadersReport {
            readers,
            numreaders: kv.num("numreaders")?,
            maxreaders: kv.num("maxreaders")?,
        })
    }

    /// The slots of process `pid`.
    pub fn of_pid(&self, pid: u32) -> Vec<&ReaderSlot> {
        self.readers
            .iter()
            .filter(|r| i64::from(r.pid) == i64::from(pid))
            .collect()
    }
}

/// The `READY txnid=.. pid=..` line of `hold-reader`.
#[derive(Clone, Debug)]
pub struct HoldReady {
    /// The pinned snapshot.
    pub txnid: u64,
    /// The holder's pid.
    pub pid: i32,
}

impl HoldReady {
    /// Parse the READY line.
    pub fn parse(line: &str) -> Result<Self> {
        let kv = Kv::from_lines([line]);
        Ok(HoldReady {
            txnid: kv.num("txnid")?,
            pid: kv.num("pid")?,
        })
    }
}

/// The last line of `writer-loop`.
#[derive(Clone, Debug)]
pub struct WriterLoopReport {
    /// Committed loop transactions.
    pub commits: u64,
    /// Aborted loop transactions.
    pub aborts: u64,
    /// Committed nested transactions.
    pub nested_commits: u64,
    /// Aborted nested transactions.
    pub nested_aborts: u64,
    /// The `#commits` value written by this writer's last commit.
    pub last_commit: u64,
}

impl WriterLoopReport {
    /// Parse the output of `writer-loop`.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::parse(text);
        Ok(WriterLoopReport {
            commits: kv.num("commits")?,
            aborts: kv.num("aborts")?,
            nested_commits: kv.num("nested_commits")?,
            nested_aborts: kv.num("nested_aborts")?,
            last_commit: kv.num("last_commit")?,
        })
    }
}

/// The last line of `reader-loop`.
#[derive(Clone, Debug)]
pub struct ReaderLoopReport {
    /// Snapshots checked.
    pub reads: u64,
    /// `#commits` of the first snapshot.
    pub first_commit: u64,
    /// `#commits` of the last snapshot.
    pub last_commit: u64,
}

impl ReaderLoopReport {
    /// Parse the output of `reader-loop`.
    pub fn parse(text: &str) -> Result<Self> {
        let kv = Kv::parse(text);
        Ok(ReaderLoopReport {
            reads: kv.num("reads")?,
            first_commit: kv.num("first_commit")?,
            last_commit: kv.num("last_commit")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tree_lines() {
        let t = TreeLine::parse(
            "db=dups flags=0x14 depth=2 branch_pages=1 leaf_pages=110 overflow_pages=0 entries=59621",
        )
        .unwrap();
        assert_eq!(t.name, "dups");
        assert_eq!(t.flags, Some(0x14));
        assert_eq!(t.leaf_pages, 110);
        assert_eq!(t.pages(), 111);
        let f = TreeLine::parse(
            "db=@free depth=1 branch_pages=0 leaf_pages=1 overflow_pages=0 entries=2",
        )
        .unwrap();
        assert_eq!(f.flags, None);
    }

    #[test]
    fn parses_readers() {
        let r = ReadersReport::parse(
            "reader pid=42 tid=0x1f txnid=7\nreader pid=43 tid=0x20 txnid=-\nreaders=2 numreaders=3 maxreaders=126\n",
        )
        .unwrap();
        assert_eq!(r.readers.len(), 2);
        assert_eq!(r.readers[0].txnid, Some(7));
        assert_eq!(r.readers[1].txnid, None);
        assert_eq!(r.of_pid(43).len(), 1);
        assert_eq!(r.numreaders, 3);
    }
}
