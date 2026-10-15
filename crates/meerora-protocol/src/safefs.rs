//! Writing into directories that a sandboxed agent can change (D16).
//!
//! An agent writes its own directory, so it can put a symlink where meerora is
//! about to write, for example `state.json.tmp -> ~/.bashrc`. The daemon and
//! the app run as the user outside the sandbox and would write through it,
//! anywhere in the home. The functions here start at a `root` that the agent
//! cannot replace, such as the agent's directory itself, and walk down with
//! `openat` and `O_NOFOLLOW`, so no part of the path below `root` may be a
//! symlink. [`replace`] writes a new temporary file, which `O_EXCL` never
//! opens through a symlink, and renames it over the old name, which replaces
//! a symlink there instead of its target.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replaces `root/rel` with `bytes`, creating the missing directories.
pub fn replace(root: &Path, rel: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let (dirs, name) = split(rel.as_ref())?;
    let dir = open_dir(root, &dirs, true)?;
    let name = cstring(name)?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..8 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let tmp = cstring(OsStr::new(&format!(".{}.{}-{n}-{nanos}.tmp", name.to_string_lossy(), std::process::id())))?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(e);
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let written = file.write_all(bytes).and_then(|()| {
            drop(file);
            check(unsafe { libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), name.as_ptr()) })
        });
        if written.is_err() {
            unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
        }
        return written;
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name for the temporary file"))
}

/// [`replace`] for a file directly in a directory that the agent cannot
/// replace, such as `agents/<id>/agent.toml`.
pub fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} names no file", path.display())));
    };
    replace(dir, name, bytes)
}

/// Appends `bytes` to `root/rel`, creating it and the missing directories.
/// A symlink at `rel` is refused, not followed.
pub fn append(root: &Path, rel: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let (dirs, name) = split(rel.as_ref())?;
    let dir = open_dir(root, &dirs, true)?;
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { File::from_raw_fd(fd) }.write_all(bytes)
}

/// Writes `root/rel` only when nothing is there, not even a symlink.
/// Returns whether it wrote.
pub fn create_new(root: &Path, rel: impl AsRef<Path>, bytes: &[u8]) -> io::Result<bool> {
    let (dirs, name) = split(rel.as_ref())?;
    let dir = open_dir(root, &dirs, true)?;
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EEXIST) { Ok(false) } else { Err(e) };
    }
    unsafe { File::from_raw_fd(fd) }.write_all(bytes)?;
    Ok(true)
}

/// The directories and the file name of `rel`, which must be relative and
/// made of plain names only.
fn split(rel: &Path) -> io::Result<(Vec<&OsStr>, &OsStr)> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a plain relative path", rel.display()))),
        }
    }
    let name = parts.pop().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path names no file"))?;
    Ok((parts, name))
}

/// Opens `root/dirs...` without following a symlink below `root`, creating
/// missing directories when `create` is set.
fn open_dir(root: &Path, dirs: &[&OsStr], create: bool) -> io::Result<OwnedFd> {
    let mut fd: OwnedFd = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open(root)?.into();
    for part in dirs {
        let name = cstring(part)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut next = unsafe { libc::openat(fd.as_raw_fd(), name.as_ptr(), flags) };
        if next < 0 && create && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            let made = unsafe { libc::mkdirat(fd.as_raw_fd(), name.as_ptr(), 0o755) };
            if made < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::last_os_error());
            }
            next = unsafe { libc::openat(fd.as_raw_fd(), name.as_ptr(), flags) };
        }
        if next < 0 {
            return Err(io::Error::last_os_error());
        }
        fd = unsafe { OwnedFd::from_raw_fd(next) };
    }
    Ok(fd)
}

fn cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds a NUL byte"))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
            let dir = std::env::temp_dir().join(format!("meerora-safefs-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An agent directory and a file outside it that the agent must not reach.
    fn setup(name: &str) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new(name);
        let agent = tmp.0.join("agent");
        let outside = tmp.0.join("home");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(".bashrc"), "mine\n").unwrap();
        (tmp, agent, outside)
    }

    #[test]
    fn a_planted_symlink_is_replaced_not_written_through() {
        let (_tmp, agent, outside) = setup("replace");
        symlink(outside.join(".bashrc"), agent.join("state.json")).unwrap();
        replace(&agent, "state.json", b"{}\n").unwrap();
        assert_eq!(std::fs::read_to_string(outside.join(".bashrc")).unwrap(), "mine\n");
        assert!(!std::fs::symlink_metadata(agent.join("state.json")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(agent.join("state.json")).unwrap(), "{}\n");
        // Nothing is left over, and replace_file does the same.
        replace_file(&agent.join("state.json"), b"[]\n").unwrap();
        let names: Vec<_> = std::fs::read_dir(&agent).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let (_tmp, agent, outside) = setup("dirs");
        symlink(&outside, agent.join("config")).unwrap();
        assert!(replace(&agent, "config/profile.json", b"{}").is_err());
        assert!(append(&agent, "config/profile.json", b"{}").is_err());
        assert!(!outside.join("profile.json").exists());
        // Missing directories are created.
        replace(&agent, "photos/a/b.png", b"png").unwrap();
        assert_eq!(std::fs::read(agent.join("photos/a/b.png")).unwrap(), b"png");
    }

    #[test]
    fn appending_refuses_a_symlink() {
        let (_tmp, agent, outside) = setup("append");
        std::fs::create_dir_all(agent.join("memory")).unwrap();
        symlink(outside.join(".bashrc"), agent.join("memory/reactions.jsonl")).unwrap();
        assert!(append(&agent, "memory/reactions.jsonl", b"{\"x\":\"$(id)\"}\n").is_err());
        assert_eq!(std::fs::read_to_string(outside.join(".bashrc")).unwrap(), "mine\n");
        append(&agent, "logs/runs.jsonl", b"a\n").unwrap();
        append(&agent, "logs/runs.jsonl", b"b\n").unwrap();
        assert_eq!(std::fs::read_to_string(agent.join("logs/runs.jsonl")).unwrap(), "a\nb\n");
    }

    #[test]
    fn create_new_writes_only_into_nothing() {
        let (_tmp, agent, outside) = setup("create");
        symlink(outside.join("new-file"), agent.join(".gitignore")).unwrap();
        assert!(!create_new(&agent, ".gitignore", b"logs/\n").unwrap());
        assert!(!outside.join("new-file").exists(), "a dangling symlink is not followed");
        assert!(create_new(&agent, "fresh", b"x").unwrap());
        assert!(!create_new(&agent, "fresh", b"y").unwrap());
        assert_eq!(std::fs::read(agent.join("fresh")).unwrap(), b"x");
    }

    #[test]
    fn only_plain_relative_paths() {
        let (_tmp, agent, _) = setup("paths");
        for bad in ["../x", "/etc/passwd", "a/../../x", ""] {
            assert!(replace(&agent, bad, b"x").is_err(), "{bad}");
        }
    }
}
