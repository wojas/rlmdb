//! What the tests do differently on Unix and Windows: positional file I/O
//! and running a program under another `argv[0]`.

use std::fs::File;
use std::io;
use std::path::Path;
use std::process::Command;

/// `pread` of all of `buf` at `off` (`FileExt::read_exact_at` on Unix,
/// `seek_read` on Windows).
pub fn read_exact_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_exact_at(file, buf, off);
    #[cfg(windows)]
    {
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(file, buf, off)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// `pwrite` of all of `buf` at `off`.
pub fn write_all_at(file: &File, buf: &[u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_all_at(file, buf, off);
    #[cfg(windows)]
    {
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_write(file, buf, off)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// A command that runs `exe` with `argv[0] = name`, so that the program's
/// messages name it as `name` (`arg0` on Unix).
///
/// On Windows `argv[0]` is the program as named: the command runs `name`,
/// from a directory put first on the child's `PATH` that has it as
/// `name.exe`: `exe`'s own directory if that is its file name, else a
/// directory next to it with a copy of `exe` under that name (the C 1.0
/// tools are `mdb_stat10.exe` & co.).
pub fn command_as(exe: &Path, name: &str) -> Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(exe);
        cmd.arg0(name);
        cmd
    }
    #[cfg(windows)]
    {
        let dir = match named_copy_dir(exe, name) {
            Ok(dir) => dir,
            Err(e) => panic!("copying {} as {name}: {e}", exe.display()),
        };
        let mut path = std::ffi::OsString::from(dir);
        if let Some(old) = std::env::var_os("PATH") {
            path.push(";");
            path.push(old);
        }
        let mut cmd = Command::new(name);
        cmd.env("PATH", path);
        cmd
    }
}

/// The directory in which `name.exe` is `exe` (see [`command_as`]).
#[cfg(windows)]
fn named_copy_dir(exe: &Path, name: &str) -> io::Result<std::path::PathBuf> {
    let parent = exe.parent().unwrap_or(Path::new("."));
    if exe.file_stem().and_then(|s| s.to_str()) == Some(name) {
        return Ok(parent.to_path_buf());
    }
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned());
    let dir = parent.join(format!("as-{name}-{}", stem.unwrap_or_default()));
    let target = dir.join(format!("{name}.exe"));
    let source = if exe.extension().is_some() {
        exe.to_path_buf()
    } else {
        exe.with_extension("exe")
    };
    let fresh = |t: &Path| -> bool {
        let (Ok(a), Ok(b)) = (std::fs::metadata(&source), std::fs::metadata(t)) else {
            return false;
        };
        a.len() == b.len() && a.modified().ok() <= b.modified().ok()
    };
    if !fresh(&target) {
        std::fs::create_dir_all(&dir)?;
        // Copied under a temporary name and renamed, for concurrent tests;
        // a copy that is running cannot be replaced, and is the same file.
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::copy(&source, &tmp)?;
        if std::fs::rename(&tmp, &target).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !target.exists() {
                return Err(io::Error::other("copy vanished"));
            }
        }
    }
    Ok(dir)
}
