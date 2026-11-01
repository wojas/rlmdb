//! A reimplementation of the C library's `getopt(3)`, as the C tools see it.
//!
//! The C tools parse their options with `getopt`, whose behaviour differs
//! between platforms, and the Rust tools must match the C tools on the same
//! platform:
//!
//! * **glibc** (Linux) permutes: options may follow operands
//!   (`mdb_stat db -e` works), unless `POSIXLY_CORRECT` is set. Its
//!   messages are `"<argv[0]>: invalid option -- 'x'"` and
//!   `"<argv[0]>: option requires an argument -- 'x'"`.
//! * **BSD** (macOS) stops at the first operand. Its messages are
//!   `"<argv[0]>: illegal option -- x"` and
//!   `"<argv[0]>: option requires an argument -- x"` (`getprogname()`
//!   returns `argv[0]` as given).
//! * **OpenBSD** parses as BSD does, with the messages
//!   `"<progname>: unknown option -- x"` and
//!   `"<progname>: option requires an argument -- x"`, where `progname` is
//!   the last component of `argv[0]` (`__progname`). **MinGW-w64**
//!   (Windows) ships OpenBSD's `getopt` and prints the same messages with
//!   `argv[0]` as given. MSVC has no `getopt`; C tools built with it here
//!   get `tests/compat/msvc/unistd.h`, which behaves the same.
//!
//! Common to both: `--` ends the options, a lone `-` is an operand, options
//! can be clustered (`-ff`), and an option argument is either the rest of
//! the cluster (`-sname`) or the next argument (`-s name`, even if that
//! starts with `-`). Arguments are bytes, as in C.

/// Which `getopt` to reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// glibc. `permute` is `false` when `POSIXLY_CORRECT` is set.
    Gnu {
        /// Whether operands may come before options.
        permute: bool,
    },
    /// The BSD `getopt` of macOS.
    Bsd,
    /// OpenBSD's `getopt` as MinGW-w64 ships it on Windows: BSD's
    /// parsing, `unknown option` messages with `argv[0]` as given.
    Mingw,
    /// OpenBSD's `getopt`: as [`Flavor::Mingw`], with the program's base
    /// name in the messages.
    OpenBsd,
}

impl Flavor {
    /// The flavour of the C library on this platform.
    pub fn native() -> Flavor {
        if cfg!(target_os = "linux") {
            Flavor::Gnu {
                permute: std::env::var_os("POSIXLY_CORRECT").is_none(),
            }
        } else if cfg!(windows) {
            Flavor::Mingw
        } else if cfg!(target_os = "openbsd") {
            Flavor::OpenBsd
        } else {
            Flavor::Bsd
        }
    }
}

/// One result of [`GetOpt::next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opt {
    /// An option without argument, e.g. `b'e'` for `-e`.
    Flag(u8),
    /// An option with its argument, e.g. `(b's', b"name")` for `-s name`.
    Arg(u8, Vec<u8>),
    /// An unknown option or a missing argument (`getopt` returns `'?'`).
    /// The message is what `getopt` writes to standard error.
    Invalid(Vec<u8>),
}

/// An option parser over `args` (`args[0]` is the program name), following
/// an `optstring` like `"Vaefnrs:"` (a letter followed by `:` takes an
/// argument).
#[derive(Debug)]
pub struct GetOpt<'a> {
    args: &'a [Vec<u8>],
    optstring: &'static [u8],
    flavor: Flavor,
    /// `optind`: the argument being scanned.
    index: usize,
    /// Position inside the current option cluster (`nextchar`); 0 when no
    /// cluster is in progress.
    pos: usize,
    /// Operands skipped while permuting (glibc).
    skipped: Vec<Vec<u8>>,
    /// Set once the end of the options was found.
    done: bool,
}

impl<'a> GetOpt<'a> {
    /// A parser with the platform's [`Flavor`].
    pub fn new(args: &'a [Vec<u8>], optstring: &'static str) -> GetOpt<'a> {
        GetOpt::with_flavor(args, optstring, Flavor::native())
    }

    /// A parser with an explicit flavour (for tests).
    pub fn with_flavor(args: &'a [Vec<u8>], optstring: &'static str, flavor: Flavor) -> GetOpt<'a> {
        GetOpt {
            args,
            optstring: optstring.as_bytes(),
            flavor,
            index: 1,
            pos: 0,
            skipped: Vec::new(),
            done: false,
        }
    }

    /// The program name as `getopt` prints it in its messages: `argv[0]`
    /// (glibc), `getprogname()` (macOS), which is also `argv[0]` as given.
    fn prog_name(&self) -> &[u8] {
        let name = self.args.first().map_or(&b""[..], |a| a.as_slice());
        match self.flavor {
            Flavor::OpenBsd => name.rsplit(|&b| b == b'/').next().unwrap_or(name),
            _ => name,
        }
    }

    /// Whether `c` is an option letter and, if so, whether it takes an
    /// argument.
    fn lookup(&self, c: u8) -> Option<bool> {
        if c == b':' || (c == b';' && matches!(self.flavor, Flavor::Gnu { .. })) {
            return None;
        }
        let i = self.optstring.iter().position(|&o| o == c)?;
        Some(self.optstring.get(i + 1) == Some(&b':'))
    }

    fn message(&self, what: &str, c: u8) -> Opt {
        let mut m = self.prog_name().to_vec();
        match self.flavor {
            Flavor::Gnu { .. } => {
                m.extend_from_slice(format!(": {what} -- '").as_bytes());
                m.push(c);
                m.extend_from_slice(b"'\n");
            }
            Flavor::Bsd | Flavor::Mingw | Flavor::OpenBsd => {
                let what = match (self.flavor, what) {
                    (Flavor::Bsd, "invalid option") => "illegal option",
                    (_, "invalid option") => "unknown option",
                    _ => what,
                };
                m.extend_from_slice(format!(": {what} -- ").as_bytes());
                m.push(c);
                m.push(b'\n');
            }
        }
        Opt::Invalid(m)
    }

    /// The next option, or `None` at the end of the options.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Opt> {
        if self.done {
            return None;
        }
        if self.pos == 0 {
            loop {
                let Some(arg) = self.args.get(self.index) else {
                    self.done = true;
                    return None;
                };
                if arg.as_slice() == b"--" {
                    self.index += 1;
                    self.done = true;
                    return None;
                }
                let is_operand = arg.len() < 2 || arg[0] != b'-';
                if !is_operand {
                    break;
                }
                match self.flavor {
                    Flavor::Gnu { permute: true } => {
                        self.skipped.push(arg.clone());
                        self.index += 1;
                    }
                    _ => {
                        self.done = true;
                        return None;
                    }
                }
            }
            self.pos = 1;
        }
        let args = self.args;
        let arg = &args[self.index];
        let c = arg[self.pos];
        self.pos += 1;
        let at_end = self.pos >= arg.len();
        match self.lookup(c) {
            None => {
                if at_end {
                    self.next_argument();
                }
                Some(self.message("invalid option", c))
            }
            Some(false) => {
                if at_end {
                    self.next_argument();
                }
                Some(Opt::Flag(c))
            }
            Some(true) => {
                if !at_end {
                    // The rest of the cluster: "-sname".
                    let value = arg[self.pos..].to_vec();
                    self.next_argument();
                    Some(Opt::Arg(c, value))
                } else if let Some(value) = args.get(self.index + 1) {
                    // The next argument: "-s name".
                    self.next_argument();
                    self.next_argument();
                    Some(Opt::Arg(c, value.clone()))
                } else {
                    self.next_argument();
                    Some(self.message("option requires an argument", c))
                }
            }
        }
    }

    fn next_argument(&mut self) {
        self.index += 1;
        self.pos = 0;
    }

    /// The operands, in order, once [`GetOpt::next`] returned `None`
    /// (`argv[optind..]` after glibc's permutation).
    pub fn operands(&self) -> Vec<Vec<u8>> {
        let mut ops = self.skipped.clone();
        ops.extend(self.args[self.index.min(self.args.len())..].iter().cloned());
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<Vec<u8>> {
        s.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    fn parse(args: &[&str], flavor: Flavor) -> (Vec<Opt>, Vec<Vec<u8>>) {
        let a = argv(args);
        let mut g = GetOpt::with_flavor(&a, "Vaefnrs:", flavor);
        let mut opts = Vec::new();
        while let Some(o) = g.next() {
            let stop = matches!(o, Opt::Invalid(_));
            opts.push(o);
            if stop {
                break;
            }
        }
        (opts, g.operands())
    }

    const GNU: Flavor = Flavor::Gnu { permute: true };

    #[test]
    fn clusters_and_arguments() {
        for flavor in [GNU, Flavor::Bsd, Flavor::Mingw, Flavor::OpenBsd] {
            let (opts, ops) = parse(&["p", "-ef", "-sname", "-s", "-x", "db"], flavor);
            assert_eq!(
                opts,
                vec![
                    Opt::Flag(b'e'),
                    Opt::Flag(b'f'),
                    Opt::Arg(b's', b"name".to_vec()),
                    Opt::Arg(b's', b"-x".to_vec()),
                ]
            );
            assert_eq!(ops, argv(&["db"]));
        }
    }

    #[test]
    fn permutation_is_gnu_only() {
        let (opts, ops) = parse(&["p", "db", "-e", "x"], GNU);
        assert_eq!(opts, vec![Opt::Flag(b'e')]);
        assert_eq!(ops, argv(&["db", "x"]));
        let (opts, ops) = parse(&["p", "db", "-e"], Flavor::Bsd);
        assert!(opts.is_empty());
        assert_eq!(ops, argv(&["db", "-e"]));
        let (opts, ops) = parse(&["p", "db", "-e"], Flavor::Gnu { permute: false });
        assert!(opts.is_empty());
        assert_eq!(ops, argv(&["db", "-e"]));
    }

    #[test]
    fn double_dash_and_lone_dash() {
        for flavor in [GNU, Flavor::Bsd, Flavor::Mingw, Flavor::OpenBsd] {
            let (opts, ops) = parse(&["p", "-e", "--", "-f"], flavor);
            assert_eq!(opts, vec![Opt::Flag(b'e')]);
            assert_eq!(ops, argv(&["-f"]));
            let (opts, ops) = parse(&["p", "-", "db"], flavor);
            assert!(opts.is_empty());
            assert_eq!(ops, argv(&["-", "db"]));
        }
        let (_, ops) = parse(&["p", "a", "--", "-e"], GNU);
        assert_eq!(ops, argv(&["a", "-e"]));
    }

    #[test]
    fn messages() {
        let (opts, _) = parse(&["/x/mdb_stat", "-ex"], GNU);
        assert_eq!(
            opts[1],
            Opt::Invalid(b"/x/mdb_stat: invalid option -- 'x'\n".to_vec())
        );
        let (opts, _) = parse(&["/x/mdb_stat", "-ex"], Flavor::Bsd);
        assert_eq!(
            opts[1],
            Opt::Invalid(b"/x/mdb_stat: illegal option -- x\n".to_vec())
        );
        // As MinGW-w64's getopt prints it (measured on Windows 10).
        let (opts, ops) = parse(&["mdb_stat", "db", "-x"], Flavor::Mingw);
        assert!(opts.is_empty());
        assert_eq!(ops, argv(&["db", "-x"]));
        let (opts, _) = parse(&["mdb_stat", "-ex"], Flavor::Mingw);
        assert_eq!(
            opts[1],
            Opt::Invalid(b"mdb_stat: unknown option -- x\n".to_vec())
        );
        let (opts, _) = parse(&["mdb_stat", "-s"], Flavor::Mingw);
        assert_eq!(
            opts[0],
            Opt::Invalid(b"mdb_stat: option requires an argument -- s\n".to_vec())
        );
        // OpenBSD's libc prints `__progname` (measured on OpenBSD 7.9).
        let (opts, ops) = parse(&["/x/mdb_stat", "db", "-x"], Flavor::OpenBsd);
        assert!(opts.is_empty());
        assert_eq!(ops, argv(&["db", "-x"]));
        let (opts, _) = parse(&["/x/mdb_stat", "-ex"], Flavor::OpenBsd);
        assert_eq!(
            opts[1],
            Opt::Invalid(b"mdb_stat: unknown option -- x\n".to_vec())
        );
        let (opts, _) = parse(&["p", "-s"], GNU);
        assert_eq!(
            opts[0],
            Opt::Invalid(b"p: option requires an argument -- 's'\n".to_vec())
        );
        let (opts, _) = parse(&["p", "-s"], Flavor::Bsd);
        assert_eq!(
            opts[0],
            Opt::Invalid(b"p: option requires an argument -- s\n".to_vec())
        );
        // "--foo" is the option '-' for both (no long options).
        let (opts, _) = parse(&["p", "--foo"], Flavor::Bsd);
        assert_eq!(opts[0], Opt::Invalid(b"p: illegal option -- -\n".to_vec()));
        let (opts, _) = parse(&["p", "-:"], GNU);
        assert_eq!(
            opts[0],
            Opt::Invalid(b"p: invalid option -- ':'\n".to_vec())
        );
    }
}
