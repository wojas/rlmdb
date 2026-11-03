//! The three libraries built from this crate (`src/profile.rs`):
//! `librlmdb`, the LMDB 0.9 stub and the LMDB 1.0 stub.
//!
//! * Each exports every function declared in LMDB 0.9's `lmdb.h`, in LMDB
//!   1.0's `lmdb.h` (rlmdb's `include/lmdb.h`) and in `rlmdb.h` (parsed at
//!   test time): `tests/c/capi_symbols.c` loads it with `dlopen`
//!   (`LoadLibrary`) and resolves each. Where `nm` runs, the exported
//!   functions are exactly those.
//! * Each reports its own version (`mdb_version`, `rlmdb_version`,
//!   `rlmdb_version_string`: rlmdb's in `librlmdb`, whose `mdb_version`
//!   string is the line of `rlmdb_version_string`; 0.9.85 and 1.0.85 in
//!   the stubs) and creates new environments in its own format when
//!   nothing asks for one (0.9, 0.9, 1.0, as `rlmdb_sniff_format` tells),
//!   while `rlmdb_env_set_format`, `rlmdb_set_default_format` and
//!   `RLMDB_DEFAULT_FORMAT` override that in all three.
//! * 0.9's functions are a subset of 1.0's; `rlmdb.h` declares rlmdb's own
//!   functions and no `mdb_*` one.
//! * On ELF systems the SONAMEs are `librlmdb.so`, `liblmdb.so.0` and
//!   `liblmdb.so`, with `DF_1_NODELETE`; on macOS the install names are
//!   `@rpath/librlmdb.dylib` and `@rpath/liblmdb.dylib`.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;
use rlmdb_capi::Profile;

/// The profile behind `artifact`.
fn profile(artifact: Artifact) -> Profile {
    match artifact {
        Artifact::Rlmdb => Profile::Rlmdb,
        Artifact::Lmdb09 => Profile::Lmdb09,
        Artifact::Lmdb10 => Profile::Lmdb10,
    }
}

/// The functions of LMDB 0.9's `lmdb.h`, LMDB 1.0's `lmdb.h` (the one in
/// `include`) and `rlmdb.h`, after checking how they relate.
fn api_functions() -> BTreeSet<String> {
    let set = |v: Vec<String>| v.into_iter().collect::<BTreeSet<_>>();
    let h09 = set(header_functions(&c_src().join("lmdb.h"), "mdb_"));
    let h10 = set(header_functions(
        &crate_dir().join("include/lmdb.h"),
        "mdb_",
    ));
    assert_eq!(
        h10,
        set(header_functions(&c10_src().join("lmdb.h"), "mdb_"))
    );
    let rlmdb_h = crate_dir().join("include/rlmdb.h");
    let ours_mdb = set(header_functions(&rlmdb_h, "mdb_"));
    let ours = set(header_functions(&rlmdb_h, "rlmdb_"));
    assert_eq!(h09.len(), 56, "{h09:?}");
    assert!(h09.is_subset(&h10), "{:?}", h09.difference(&h10));
    let added: BTreeSet<String> = h10.difference(&h09).cloned().collect();
    assert_eq!(added.len(), 13, "{added:?}");
    assert!(ours_mdb.is_empty(), "rlmdb.h declares {ours_mdb:?}");
    assert_eq!(
        ours,
        set([
            "rlmdb_env_get_format",
            "rlmdb_env_set_format",
            "rlmdb_env_set_pagesize",
            "rlmdb_set_default_format",
            "rlmdb_sniff_format",
            "rlmdb_version",
            "rlmdb_version_string",
        ]
        .map(String::from)
        .to_vec())
    );
    h10.union(&ours).cloned().collect()
}

/// Runs `capi_symbols` on `artifact`'s shared library with
/// `RLMDB_DEFAULT_FORMAT` set to `var` (unset for `None`) and returns its
/// `R ...` lines as (what, value) pairs.
fn run_capi_symbols(artifact: Artifact, var: Option<&str>) -> Vec<(String, String)> {
    let b = builder();
    let exe = b.dl_program("capi_symbols");
    let lib = artifact_cdylib(artifact);
    let tag = format!("{}-{}", artifact.lib_name(), var.unwrap_or("unset"));
    let scratch = b.scratch(&format!("symbols-{tag}"));
    let names = scratch.join("names.txt");
    let api = api_functions();
    let text: String = api.iter().map(|n| format!("{n}\n")).collect();
    fs::write(&names, text).unwrap();
    let mut cmd = Command::new(&exe);
    cmd.arg(&lib).arg(&scratch).arg(&names);
    match var {
        Some(v) => cmd.env(rlmdb::DEFAULT_FORMAT_ENV, v),
        None => cmd.env_remove(rlmdb::DEFAULT_FORMAT_ENV),
    };
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "--- capi_symbols {} ({tag}) ---\n{stdout}{stderr}",
        lib.display()
    );
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_symbols {} failed ({})",
        lib.display(),
        out.status
    );
    assert!(
        stdout.contains(&format!("resolved {}\n", api.len())),
        "{stdout}"
    );
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("R "))
        .map(|l| {
            let (what, value) = l.split_once(' ').unwrap();
            (what.to_owned(), value.to_owned())
        })
        .collect()
}

/// The value of `what` in `results`.
fn value<'a>(results: &'a [(String, String)], what: &str) -> &'a str {
    results
        .iter()
        .find(|(w, _)| w == what)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {what} in {results:?}"))
}

#[test]
fn every_library_exports_the_whole_api_and_reports_its_profile() {
    for artifact in Artifact::ALL {
        let r = run_capi_symbols(artifact, None);
        let (ma, mi, pa) = artifact.lmdb_version();
        let v = rlmdb_capi::version::version_info(profile(artifact));
        assert_eq!(v.lmdb, (ma, mi, pa));
        let rlmdb = env!("CARGO_PKG_VERSION");
        let string = match artifact {
            // librlmdb: rlmdb's line, as rlmdb_version_string.
            Artifact::Rlmdb => v.to_string(),
            Artifact::Lmdb09 | Artifact::Lmdb10 => format!("LMDB {ma}.{mi}.{pa}: (rlmdb {rlmdb})"),
        };
        assert_eq!(
            value(&r, "version"),
            format!("{ma}.{mi}.{pa} {string}"),
            "{artifact:?}"
        );
        assert_eq!(value(&r, "info"), format!("{ma}.{mi}.{pa}"));
        assert_eq!(value(&r, "line"), v.to_string());
        assert!(
            value(&r, "line").starts_with(&format!("rlmdb {rlmdb} (")),
            "{r:?}"
        );
        assert!(
            value(&r, "line").contains(&format!("(mdb_version {ma}.{mi}.{pa};")),
            "{r:?}"
        );
        let default = artifact.default_format().to_string();
        assert_eq!(value(&r, "default"), default, "{artifact:?}");
        assert_eq!(value(&r, "request09"), "9", "{artifact:?}");
        assert_eq!(value(&r, "request10"), "10", "{artifact:?}");
        assert_eq!(value(&r, "process09"), "9", "{artifact:?}");
        assert_eq!(value(&r, "process10"), "10", "{artifact:?}");
        assert_eq!(value(&r, "explicit"), "9", "{artifact:?}");
        assert_eq!(value(&r, "reset"), default, "{artifact:?}");
    }
}

#[test]
fn the_default_format_variable_overrides_every_library() {
    for artifact in Artifact::ALL {
        for (var, format) in [("0.9", "9"), ("1.0", "10")] {
            let r = run_capi_symbols(artifact, Some(var));
            assert_eq!(value(&r, "default"), format, "{artifact:?} {var}");
            assert_eq!(value(&r, "reset"), format, "{artifact:?} {var}");
            // A request and the process default come first.
            assert_eq!(value(&r, "request09"), "9", "{artifact:?} {var}");
            assert_eq!(value(&r, "request10"), "10", "{artifact:?} {var}");
            assert_eq!(value(&r, "process09"), "9", "{artifact:?} {var}");
            assert_eq!(value(&r, "process10"), "10", "{artifact:?} {var}");
        }
    }
}

/// The function symbols `lib` exports, as `nm` lists them (without
/// macOS's leading underscore); `None` with a note where `nm` cannot be
/// run.
fn exported_functions(lib: &Path) -> Option<BTreeSet<String>> {
    let args: &[&str] = if cfg!(target_os = "macos") {
        &["-gU"]
    } else {
        &["-D", "--defined-only"]
    };
    let out = match Command::new("nm").args(args).arg(lib).output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "NOTE: nm {}: {}",
                lib.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Err(e) => {
            eprintln!("NOTE: nm cannot be run ({e}): exported symbols not listed");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let (_, kind, name) = (w.next()?, w.next()?, w.next()?);
                (kind == "T").then_some(name)
            })
            .map(|n| {
                n.strip_prefix('_')
                    .filter(|_| cfg!(target_os = "macos"))
                    .unwrap_or(n)
            })
            .map(String::from)
            .collect(),
    )
}

#[test]
fn the_exported_functions_are_exactly_the_api() {
    if cfg!(windows) {
        eprintln!("NOTE: no nm on Windows: the dlsym test checks the exports");
        return;
    }
    let api = api_functions();
    for artifact in Artifact::ALL {
        let lib = artifact_cdylib(artifact);
        let Some(exported) = exported_functions(&lib) else {
            return;
        };
        assert_eq!(
            exported,
            api,
            "{}: missing {:?}, extra {:?}",
            lib.display(),
            api.difference(&exported).collect::<Vec<_>>(),
            exported.difference(&api).collect::<Vec<_>>()
        );
    }
}

// ---------------------------------------------------------------------------
// SONAME and install name
// ---------------------------------------------------------------------------

/// What the dynamic section of an ELF shared library says: its SONAME
/// and whether `DF_1_NODELETE` is set.
#[derive(Debug, PartialEq, Eq)]
struct ElfDynamic {
    soname: Option<String>,
    nodelete: bool,
}

/// Parses the dynamic section of the 64-bit ELF file `data` (either byte
/// order), through its program headers: `PT_DYNAMIC` for the entries,
/// `PT_LOAD` to find the string table `DT_STRTAB` points to.
fn elf_dynamic(data: &[u8]) -> Result<ElfDynamic, String> {
    if data.get(..4) != Some(b"\x7fELF".as_slice()) || data.get(4) != Some(&2) {
        return Err("not a 64-bit ELF file".into());
    }
    let big = data.get(5) == Some(&2);
    let u = |off: usize, n: usize| -> Result<u64, String> {
        let b = data
            .get(off..off + n)
            .ok_or_else(|| format!("truncated at {off}"))?;
        let mut v = 0u64;
        for i in 0..n {
            let byte = if big { b[i] } else { b[n - 1 - i] };
            v = (v << 8) | u64::from(byte);
        }
        Ok(v)
    };
    let (phoff, phentsize, phnum) = (u(0x20, 8)? as usize, u(0x36, 2)? as usize, u(0x38, 2)?);
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum as usize {
        let ph = phoff + i * phentsize;
        let (kind, offset, vaddr, filesz) =
            (u(ph, 4)?, u(ph + 8, 8)?, u(ph + 16, 8)?, u(ph + 32, 8)?);
        match kind {
            1 => loads.push((vaddr, offset, filesz)),
            2 => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic.ok_or("no PT_DYNAMIC")?;
    let (mut strtab, mut soname, mut flags_1) = (None, None, 0);
    for i in 0..(dyn_size / 16) as usize {
        let e = dyn_off as usize + i * 16;
        let (tag, val) = (u(e, 8)?, u(e + 8, 8)?);
        match tag {
            0 => break,
            5 => strtab = Some(val),
            14 => soname = Some(val),
            0x6fff_fffb => flags_1 = val,
            _ => {}
        }
    }
    let soname = match (strtab, soname) {
        (Some(strtab), Some(name)) => {
            let (vaddr, offset, _) = loads
                .iter()
                .find(|(v, _, size)| (*v..v + size).contains(&strtab))
                .ok_or("DT_STRTAB outside the PT_LOAD segments")?;
            let start = (strtab - vaddr + offset + name) as usize;
            let rest = data.get(start..).ok_or("DT_SONAME outside the file")?;
            let end = rest.iter().position(|&b| b == 0).ok_or("unterminated")?;
            Some(String::from_utf8_lossy(&rest[..end]).into_owned())
        }
        _ => None,
    };
    Ok(ElfDynamic {
        soname,
        nodelete: flags_1 & 0x8 != 0,
    })
}

/// The SONAME `readelf -d` prints for `lib`, if it can be run and prints
/// it as GNU's does (`(SONAME) ... [name]`).
fn readelf_soname(lib: &Path) -> Option<String> {
    let out = Command::new("readelf").arg("-d").arg(lib).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("(SONAME)"))?;
    let start = line.find('[')? + 1;
    let end = line.rfind(']')?;
    Some(line[start..end].to_owned())
}

#[test]
fn shared_library_names() {
    let elf = cfg!(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ));
    for artifact in Artifact::ALL {
        let lib = artifact_cdylib(artifact);
        if elf {
            let d = elf_dynamic(&fs::read(&lib).unwrap()).unwrap();
            assert_eq!(
                d,
                ElfDynamic {
                    soname: Some(artifact.soname().to_owned()),
                    nodelete: true,
                },
                "{}",
                lib.display()
            );
            match readelf_soname(&lib) {
                Some(s) => assert_eq!(s, artifact.soname(), "readelf -d {}", lib.display()),
                None => {
                    eprintln!("NOTE: no GNU readelf output: the SONAME was read by the test only")
                }
            }
        } else if cfg!(target_os = "macos") {
            let expected = match artifact {
                Artifact::Rlmdb => "@rpath/librlmdb.dylib",
                Artifact::Lmdb09 | Artifact::Lmdb10 => "@rpath/liblmdb.dylib",
            };
            match Command::new("otool").arg("-D").arg(&lib).output() {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    assert_eq!(text.lines().nth(1), Some(expected), "{text}");
                }
                _ => eprintln!("NOTE: otool cannot be run: install names not checked"),
            }
        } else {
            eprintln!("NOTE: {}: a DLL is found by its file name", lib.display());
        }
    }
}
