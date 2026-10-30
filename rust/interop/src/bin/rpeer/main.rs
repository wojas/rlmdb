//! `rpeer`: the Rust implementation of the interop peer protocol.
//!
//! ```text
//! rpeer <command> <envpath> [args...] [options...]
//! ```
//!
//! The protocol (commands, arguments, output lines, exit codes) is
//! documented in `rust/interop/README.md`; the C peer (`c/cpeer*.c`) is the
//! reference implementation, and this program follows it module by module:
//!
//! | module | C file | commands |
//! |---|---|---|
//! | `args` | `cpeer.c` | option parsing, exit codes, env open |
//! | `dataset` | `dataset.c` | the deterministic data set |
//! | `data` | `cpeer_data.c` | `fill`, `verify`, `delete`, `drop`, `dump` |
//! | `loops` | `cpeer_loops.c` | `hold-reader`, `hold-env`, `hold-writer`, `writer-loop`, `reader-loop`, `formats-loop` (rpeer only) |
//! | `misc` | `cpeer_misc.c` | `create`, `stat`, `readers`, `reader-check`, `copy`, `nested-test`, `gen` |
//! | `pagewalk` | `cpeer_pagewalk.c` | `check-freelist` |
//!
//! It uses only the public API of the `lmdb` crate: `rlmdb::core` for
//! everything except `check-freelist`, which decodes the data file itself
//! with `rlmdb::format`.
//!
//! Commands that need a part of `rlmdb` that does not exist yet parse
//! their arguments and then exit with status 4 ("not implemented in rpeer
//! yet"); `rpeer commands` lists what is implemented, which is how the
//! harness decides to skip or substitute (`interop::Peer::supports`).

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all)]

mod args;
mod data;
mod dataset;
mod loops;
mod misc;
mod pagewalk;

use std::process::ExitCode;

use args::{CmdResult, Fail, Opts, EXIT_USAGE};

/// One command of the protocol (`struct command`).
struct Command {
    name: &'static str,
    run: fn(&mut Opts) -> CmdResult,
    /// Allowed number of positional arguments, including `<env>`.
    min_pos: usize,
    max_pos: usize,
    usage: &'static str,
    /// `false` for commands that need a part of `rlmdb` that does not
    /// exist yet (exit status 4).
    implemented: bool,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "create",
        run: misc::cmd_create,
        min_pos: 3,
        max_pos: 3,
        usage: "create <env> <mapsize> <maxdbs>",
        implemented: true,
    },
    Command {
        name: "fill",
        run: data::cmd_fill,
        min_pos: 4,
        max_pos: 4,
        usage: "fill <env> <db|-> <count> <seed> [data options]",
        implemented: true,
    },
    Command {
        name: "verify",
        run: data::cmd_verify,
        min_pos: 4,
        max_pos: 4,
        usage: "verify <env> <db|-> <count> <seed> [data options]",
        implemented: true,
    },
    Command {
        name: "delete",
        run: data::cmd_delete,
        min_pos: 4,
        max_pos: 4,
        usage: "delete <env> <db|-> <count> <seed> [--every N] [data options]",
        implemented: true,
    },
    Command {
        name: "drop",
        run: data::cmd_drop,
        min_pos: 2,
        max_pos: 2,
        usage: "drop <env> <db|-> [--delete]",
        implemented: true,
    },
    Command {
        name: "dump",
        run: data::cmd_dump,
        min_pos: 1,
        max_pos: 2,
        usage: "dump <env> <db|-> | dump <env> --all",
        implemented: true,
    },
    Command {
        name: "stat",
        run: misc::cmd_stat,
        min_pos: 1,
        max_pos: 1,
        usage: "stat <env>",
        implemented: true,
    },
    Command {
        name: "readers",
        run: misc::cmd_readers,
        min_pos: 1,
        max_pos: 1,
        usage: "readers <env>",
        implemented: true,
    },
    Command {
        name: "reader-check",
        run: misc::cmd_reader_check,
        min_pos: 1,
        max_pos: 1,
        usage: "reader-check <env>",
        implemented: true,
    },
    Command {
        name: "hold-reader",
        run: loops::cmd_hold_reader,
        min_pos: 2,
        max_pos: 5,
        usage: "hold-reader <env> <seconds> [--until-stdin] [<db|-> <count> <seed> [data options]]",
        implemented: true,
    },
    Command {
        name: "hold-env",
        run: loops::cmd_hold_env,
        min_pos: 2,
        max_pos: 2,
        usage: "hold-env <env> <seconds> [--until-stdin]",
        implemented: true,
    },
    Command {
        name: "hold-writer",
        run: loops::cmd_hold_writer,
        min_pos: 2,
        max_pos: 2,
        usage: "hold-writer <env> <seconds> [--until-stdin]",
        implemented: true,
    },
    Command {
        name: "writer-loop",
        run: loops::cmd_writer_loop,
        min_pos: 4,
        max_pos: 4,
        usage: "writer-loop <env> <db|-> <seconds> <seed> [--keys N] [--max-commits N]",
        implemented: true,
    },
    Command {
        name: "reader-loop",
        run: loops::cmd_reader_loop,
        min_pos: 3,
        max_pos: 3,
        usage: "reader-loop <env> <db|-> <seconds>",
        implemented: true,
    },
    Command {
        name: "formats-loop",
        run: loops::cmd_formats_loop,
        min_pos: 5,
        max_pos: 5,
        usage: "formats-loop <env 0.9> <env 1.0> <db|-> <seconds> <seed> [--keys N] \
                [--max-commits N]",
        implemented: true,
    },
    Command {
        name: "check-freelist",
        run: pagewalk::cmd_check_freelist,
        min_pos: 1,
        max_pos: 1,
        usage: "check-freelist <env>",
        implemented: true,
    },
    Command {
        name: "nested-test",
        run: misc::cmd_nested_test,
        min_pos: 1,
        max_pos: 1,
        usage: "nested-test <env> [--check]",
        implemented: true,
    },
    Command {
        name: "copy",
        run: misc::cmd_copy,
        min_pos: 2,
        max_pos: 2,
        usage: "copy <env> <dest> [--compact]",
        implemented: true,
    },
    Command {
        name: "gen",
        run: misc::cmd_gen,
        min_pos: 2,
        max_pos: 2,
        usage: "gen <seed> <i> [data options]",
        implemented: true,
    },
    Command {
        name: "commands",
        run: cmd_commands,
        min_pos: 0,
        max_pos: 0,
        usage: "commands",
        implemented: true,
    },
];

/// `commands`: `command=<name> implemented=<0|1>` for every command.
fn cmd_commands(_o: &mut Opts) -> CmdResult {
    for c in COMMANDS {
        println!("command={} implemented={}", c.name, u8::from(c.implemented));
    }
    Ok(())
}

/// The usage text, as a [`EXIT_USAGE`] failure.
fn usage() -> Fail {
    let mut s = String::from("usage: rpeer <command> <args...> [options]\ncommands:\n");
    for c in COMMANDS {
        s.push_str("  ");
        s.push_str(c.usage);
        s.push('\n');
    }
    s.push_str(
        "env options: --nosubdir --notls --nosync --nometasync --mapsize N --maxdbs N \
         --maxreaders N --format 0.9|1.0 --pagesize N\n\
         data options: --dupsort --dupfixed --intkey --intdup --revkey --revdup --valsize N --big N \
         --dups N --deleted-every N --per-txn N --append --reserve\n\
         command options: --every N --keys N --max-commits N --compact --all --until-stdin \
         --check --delete",
    );
    Fail::usage(s)
}

/// Parse the command line and run the command. Everything the command
/// opened is dropped (environments closed, reader slots released) before
/// this returns.
fn run() -> CmdResult {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .map_err(|a| Fail::usage(format!("argument is not UTF-8: {a:?}")))
        })
        .collect::<CmdResult<_>>()?;
    if args.is_empty() {
        return Err(usage());
    }
    let mut o = Opts::parse(&args)?;
    let Some(c) = COMMANDS.iter().find(|c| c.name == o.cmd) else {
        if o.cmd != "help" && o.cmd != "--help" {
            eprintln!("rpeer: unknown command '{}'", o.cmd);
        }
        return Err(usage());
    };
    if o.pos.len() < c.min_pos || o.pos.len() > c.max_pos {
        return Err(Fail::new(EXIT_USAGE, format!("usage: rpeer {}", c.usage)));
    }
    (c.run)(&mut o)
}

fn main() -> ExitCode {
    // Rust's stdout is line-buffered, so READY lines reach the harness at
    // once, as with C's setvbuf(_IOLBF).
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(fail) => {
            if let Some(msg) = &fail.message {
                eprintln!("rpeer: {msg}");
            }
            ExitCode::from(fail.code)
        }
    }
}
