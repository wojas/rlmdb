//! `lmdb-bench`: the Rust LMDB rewrite against the C library, same
//! workloads, same data, side by side. See `README.md` for the workloads
//! and how to run it.
//!
//! The driver runs every workload group once per implementation, each in a
//! fresh process: `cbench` (the C program built by `build.rs`) and this
//! binary in worker mode (`--worker <group> <dir> <n> <reps>
//! <sync_commits>`, the same arguments as `cbench`). Workers print
//! `RESULT <name> <ns>/<ops> ...` lines, one pair per timed repetition; the
//! driver collects them and prints a markdown table with the median and the
//! minimum time per operation of each side and the ratio of the medians.
//!
//! With `--baseline <lmdb-bench>` the other side is not C but the Rust side
//! of another build of this program (say, of the commit before a change):
//! an A/B comparison of two versions of `rlmdb`, best run with several
//! `--rounds` so the two alternate.

mod data;
mod workloads;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The workload groups, in the order they run.
const GROUPS: &[&str] = &[
    "seq_put",
    "rand_put",
    "read",
    "overwrite",
    "dupsort",
    "overflow",
    "delete",
    "small_txn",
    "mixed",
    "ls",
];

/// The path of the C benchmark program.
const CBENCH: &str = env!("LMDB_BENCH_CBENCH");

const USAGE: &str = "\
usage: lmdb-bench [options]
  --n N              number of keys (default 1000000; other sizes scale with it)
  --reps R           timed repetitions per workload, after one warm-up (default 5)
  --sync-commits K   commits of txn_sync (default 1000)
  --dir D            where to create the environments (default: the temp dir)
  --only G1,G2       run only these groups (default: all)
  --impl both|c|rust (default both)
  --raw              also print the raw RESULT lines
  --baseline EXE     compare with the Rust side of another lmdb-bench build
                     (A/B of two rlmdb versions) instead of with C
  --rounds K         run every group K times per side, alternating (default 1)
  --check-data       check that C and Rust generate the same data, and exit
groups: seq_put rand_put read overwrite dupsort overflow delete small_txn mixed ls";

struct Options {
    n: usize,
    reps: usize,
    sync_commits: usize,
    dir: PathBuf,
    groups: Vec<String>,
    run_c: bool,
    run_rust: bool,
    raw: bool,
    baseline: Option<PathBuf>,
    rounds: usize,
    check_data: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--worker") {
        worker(&args[1..]);
        return;
    }
    let opts = parse(&args).unwrap_or_else(|e| {
        eprintln!("lmdb-bench: {e}\n{USAGE}");
        std::process::exit(2);
    });
    if opts.check_data {
        check_data(opts.n);
    } else {
        driver(&opts);
    }
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        n: 1_000_000,
        reps: 5,
        sync_commits: 1000,
        dir: std::env::temp_dir().join(format!("lmdb-bench-{}", std::process::id())),
        groups: GROUPS.iter().map(|g| g.to_string()).collect(),
        run_c: true,
        run_rust: true,
        raw: false,
        baseline: None,
        rounds: 1,
        check_data: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        let num = |v: &String| v.parse::<usize>().map_err(|e| format!("{a} {v}: {e}"));
        match a.as_str() {
            "--n" => o.n = num(val()?)?,
            "--reps" => o.reps = num(val()?)?,
            "--sync-commits" => o.sync_commits = num(val()?)?,
            "--dir" => o.dir = PathBuf::from(val()?),
            "--only" => {
                o.groups = val()?.split(',').map(str::to_string).collect();
                if let Some(g) = o.groups.iter().find(|g| !GROUPS.contains(&g.as_str())) {
                    return Err(format!("unknown group {g}"));
                }
            }
            "--impl" => match val()?.as_str() {
                "both" => (o.run_c, o.run_rust) = (true, true),
                "c" => (o.run_c, o.run_rust) = (true, false),
                "rust" => (o.run_c, o.run_rust) = (false, true),
                v => return Err(format!("--impl {v}: expected both, c or rust")),
            },
            "--raw" => o.raw = true,
            "--baseline" => o.baseline = Some(PathBuf::from(val()?)),
            "--rounds" => o.rounds = num(val()?)?.max(1),
            "--check-data" => o.check_data = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    if o.n < 2000 || !(1..=64).contains(&o.reps) {
        return Err("need --n >= 2000 and 1 <= --reps <= 64".into());
    }
    Ok(o)
}

/// Worker mode: run one group of the Rust side.
fn worker(args: &[String]) {
    let [group, dir, n, reps, sync_commits] = args else {
        eprintln!("usage: lmdb-bench --worker <group> <dir> <n> <reps> <sync_commits>");
        std::process::exit(2);
    };
    let num = |s: &str| s.parse::<usize>().expect("numeric argument");
    let p = workloads::Params {
        dir: Path::new(dir),
        n: num(n),
        reps: num(reps),
        sync_commits: num(sync_commits),
    };
    if let Err(e) = workloads::run(group, &p) {
        eprintln!("lmdb-bench: {group}: {e}");
        std::process::exit(1);
    }
}

/// `--check-data`: the `datacheck` group on both sides must print the
/// same hash of the generated keys, values, index streams and
/// permutations.
fn check_data(n: usize) {
    let dir = std::env::temp_dir().join(format!("lmdb-bench-check-{}", std::process::id()));
    let args = [
        "datacheck".to_string(),
        dir.display().to_string(),
        n.to_string(),
        "1".to_string(),
        "1".to_string(),
    ];
    let run = |cmd: &mut Command| {
        let out = cmd
            .args(&args)
            .output()
            .expect("running a datacheck worker");
        assert!(
            out.status.success(),
            "datacheck worker failed: {}",
            out.status
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let c = run(&mut Command::new(CBENCH));
    let rust = run(Command::new(std::env::current_exe().expect("current_exe")).arg("--worker"));
    let _ = std::fs::remove_dir_all(&dir);
    println!("C:    {c}\nRust: {rust}");
    if c != rust || c.is_empty() {
        println!("lmdb-bench: the C and Rust data generators differ");
        std::process::exit(1);
    }
    println!("same data on both sides (n = {n})");
}

/// Samples of one workload on one side: `(ns, ops)` per repetition.
type Samples = Vec<(u64, u64)>;

/// The samples of one workload: `other` is C (or the `--baseline` build),
/// `rust` this build.
#[derive(Default)]
struct Row {
    other: Option<Samples>,
    rust: Option<Samples>,
}

fn driver(o: &Options) {
    std::fs::create_dir_all(&o.dir).expect("create --dir");
    print_machine_info(o);
    let other = if o.baseline.is_some() { "base" } else { "c" };
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for round in 0..o.rounds {
        for (gi, group) in o.groups.iter().enumerate() {
            // Alternate which side goes first, so slow drift (thermals,
            // page cache, other load) does not always favour the same one.
            let mut sides = vec![];
            if o.run_c {
                sides.push(true);
            }
            if o.run_rust {
                sides.push(false);
            }
            if (gi + round) % 2 == 1 {
                sides.reverse();
            }
            for is_other in sides {
                let side = if is_other { other } else { "rust" };
                let dir = o.dir.join(format!("{side}-{group}"));
                let args = [
                    group.clone(),
                    dir.display().to_string(),
                    o.n.to_string(),
                    o.reps.to_string(),
                    o.sync_commits.to_string(),
                ];
                let mut cmd = match (is_other, &o.baseline) {
                    (true, None) => Command::new(CBENCH),
                    (true, Some(exe)) => {
                        let mut c = Command::new(exe);
                        c.arg("--worker");
                        c
                    }
                    (false, _) => {
                        let mut c = Command::new(std::env::current_exe().expect("current_exe"));
                        c.arg("--worker");
                        c
                    }
                };
                eprintln!("lmdb-bench: {group} ({side}) ...");
                let out = cmd
                    .args(&args)
                    .stderr(Stdio::inherit())
                    .output()
                    .unwrap_or_else(|e| panic!("running the {side} worker: {e}"));
                if !out.status.success() {
                    eprintln!(
                        "lmdb-bench: the {side} worker for {group} failed: {}",
                        out.status
                    );
                    std::process::exit(1);
                }
                let _ = std::fs::remove_dir_all(&dir);
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let Some((name, samples)) = parse_result(line) else {
                        continue;
                    };
                    if o.raw {
                        println!("{side:4} {line}");
                    }
                    if !rows.contains_key(&name) {
                        order.push(name.clone());
                    }
                    let row = rows.entry(name).or_default();
                    let slot = if is_other {
                        &mut row.other
                    } else {
                        &mut row.rust
                    };
                    slot.get_or_insert_with(Vec::new).extend(samples);
                }
            }
        }
    }
    let _ = std::fs::remove_dir(&o.dir);
    print_table(o, &order, &rows);
}

fn parse_result(line: &str) -> Option<(String, Samples)> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "RESULT" {
        return None;
    }
    let name = parts.next()?.to_string();
    let samples = parts
        .map(|p| {
            let (ns, ops) = p.split_once('/')?;
            Some((ns.parse().ok()?, ops.parse().ok()?))
        })
        .collect::<Option<Samples>>()?;
    Some((name, samples))
}

/// Median and minimum nanoseconds per operation, and the median number of
/// operations per repetition.
fn stats(s: &Samples) -> (f64, f64, u64) {
    let mut per_op: Vec<f64> = s
        .iter()
        .map(|&(ns, ops)| ns as f64 / ops.max(1) as f64)
        .collect();
    per_op.sort_by(f64::total_cmp);
    let mut ops: Vec<u64> = s.iter().map(|&(_, ops)| ops).collect();
    ops.sort_unstable();
    let mid = per_op.len() / 2;
    let median = if per_op.len() % 2 == 0 {
        (per_op[mid - 1] + per_op[mid]) / 2.0
    } else {
        per_op[mid]
    };
    (median, per_op[0], ops[ops.len() / 2])
}

fn fmt_ns(ns: f64) -> String {
    if ns >= 100.0 {
        format!("{ns:.0}")
    } else {
        format!("{ns:.1}")
    }
}

fn fmt_rate(ns_per_op: f64) -> String {
    let per_s = 1e9 / ns_per_op;
    if per_s >= 1e6 {
        format!("{:.2}M", per_s / 1e6)
    } else if per_s >= 1e3 {
        format!("{:.1}k", per_s / 1e3)
    } else {
        format!("{:.0}", per_s)
    }
}

fn print_table(o: &Options, order: &[String], rows: &BTreeMap<String, Row>) {
    let (other, rust) = if o.baseline.is_some() {
        ("base", "new")
    } else {
        ("C", "Rust")
    };
    println!();
    println!(
        "n = {}, {} round(s) of {} timed reps after 1 warm-up; ns/op: median (min); \
         ratio = {rust} median / {other} median (> 1: {rust} slower)",
        o.n, o.rounds, o.reps
    );
    if let Some(b) = &o.baseline {
        println!();
        println!("base = Rust side of {}", b.display());
    }
    println!();
    println!(
        "| workload | ops/rep | {other} ns/op | {rust} ns/op | {other} ops/s | {rust} ops/s | {rust}/{other} |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for name in order {
        let row = &rows[name];
        let c = row.other.as_ref().map(stats);
        let r = row.rust.as_ref().map(stats);
        let cell = |s: Option<(f64, f64, u64)>| match s {
            Some((med, min, _)) => format!("{} ({})", fmt_ns(med), fmt_ns(min)),
            None => "-".into(),
        };
        let rate = |s: Option<(f64, f64, u64)>| s.map_or("-".into(), |(med, _, _)| fmt_rate(med));
        let ops = c.or(r).map_or(0, |s| s.2);
        let ratio = match (c, r) {
            (Some(c), Some(r)) => format!("{:.2}", r.0 / c.0),
            _ => "-".into(),
        };
        println!(
            "| {name} | {ops} | {} | {} | {} | {} | {ratio} |",
            cell(c),
            cell(r),
            rate(c),
            rate(r)
        );
    }
}

/// Prints the machine, OS, compiler versions and page size (to stdout, as
/// part of the report).
fn print_machine_info(o: &Options) {
    let sh = |cmd: &str| -> String {
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    };
    println!("## lmdb-bench");
    println!();
    println!("* uname: `{}`", sh("uname -srvm"));
    if cfg!(target_os = "macos") {
        println!(
            "* CPU: {}, {} cores, {} GiB RAM, macOS {}",
            sh("sysctl -n machdep.cpu.brand_string"),
            sh("sysctl -n hw.ncpu"),
            sh("sysctl -n hw.memsize").parse::<u64>().unwrap_or(0) >> 30,
            sh("sw_vers -productVersion")
        );
    } else {
        println!(
            "* CPU: {} ({} cores), {}",
            sh("(lscpu 2>/dev/null | sed -n 's/^Model name: *//p'; uname -m) | head -1"),
            sh("nproc"),
            sh("grep MemTotal /proc/meminfo")
        );
    }
    println!(
        "* OS page size (= LMDB page size): {}",
        sh("getconf PAGESIZE")
    );
    println!("* rustc: {}", env!("LMDB_BENCH_RUSTC_VERSION"));
    println!("* cc: {}", env!("LMDB_BENCH_CC_VERSION"));
    println!("* environments in: {}", o.dir.display());
    println!();
}
