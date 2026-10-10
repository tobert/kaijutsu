//! Host-side access to a workspace a model can write.
//!
//! A contained model can plant a symbolic link anywhere in its workspace and
//! aim it at any host path, such as `ln -s ~/.ssh/id_ed25519 hello.txt`. The
//! runner reads, writes, and checks workspace paths on the host, so every
//! such access goes through here, and each refuses a path with a symbolic
//! link in any component, the last one or a directory on the way.
//! `openat2` with `RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH` resolves the whole
//! path in one call, so a link swapped in while the agent runs is refused
//! too. A refusal names the link and never reads through it. A read also
//! refuses anything but a regular file, so a planted FIFO cannot block the
//! runner, and a file larger than [`READ_LIMIT`].

use std::fs::File;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// The most bytes [`read`] returns.
pub const READ_LIMIT: u64 = 1 << 20;

/// Whether `relative` exists under `root`. A missing parent reads as not
/// existing.
pub fn exists(root: &Path, relative: &str) -> Result<bool> {
    match open(root, relative, OFlags::PATH, Mode::empty()) {
        Ok(_) => Ok(true),
        Err(Refused::Io(Errno::NOENT | Errno::NOTDIR)) => Ok(false),
        Err(refused) => Err(refused.into_error(root, relative)),
    }
}

/// The UTF-8 text of the regular file `relative` under `root`.
pub fn read(root: &Path, relative: &str) -> Result<String> {
    let fd = open(root, relative, OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY, Mode::empty())
        .map_err(|r| r.into_error(root, relative))?;
    let stat = regular(&fd, relative)?;
    if stat.st_size as u64 > READ_LIMIT {
        bail!("{relative} is {} bytes, larger than the {READ_LIMIT} the runner reads", stat.st_size);
    }
    let mut bytes = Vec::new();
    File::from(fd).take(READ_LIMIT + 1).read_to_end(&mut bytes).with_context(|| format!("read {relative}"))?;
    if bytes.len() as u64 > READ_LIMIT {
        bail!("{relative} grew larger than the {READ_LIMIT} bytes the runner reads");
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{relative} is not UTF-8 text"))
}

/// Create the regular file `relative` under `root`, or empty it if it
/// exists. Its directory must exist.
pub fn write_empty(root: &Path, relative: &str) -> Result<()> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NONBLOCK | OFlags::NOCTTY;
    let fd = open(root, relative, flags, Mode::from_raw_mode(0o644)).map_err(|r| r.into_error(root, relative))?;
    regular(&fd, relative)?;
    Ok(())
}

enum Refused {
    /// The root itself could not be opened.
    Root(std::io::Error),
    Io(Errno),
}

impl Refused {
    fn into_error(self, root: &Path, relative: &str) -> anyhow::Error {
        match self {
            Self::Root(error) => anyhow!("open the workspace {}: {error}", root.display()),
            Self::Io(Errno::LOOP) => match first_link(root, relative) {
                Some(link) => anyhow!(
                    "refused: {} is a symbolic link, and the runner never follows a link in the workspace, \
                     where the model may have aimed it at a host file",
                    link.display()
                ),
                None => anyhow!("refused: {relative} resolves through a symbolic link, which the runner never follows"),
            },
            Self::Io(Errno::XDEV) => anyhow!("refused: {relative} resolves outside the workspace"),
            Self::Io(errno) => anyhow!("{relative}: {}", std::io::Error::from(errno)),
        }
    }
}

/// Open `relative` beneath `root` with no symbolic link in any component.
fn open(root: &Path, relative: &str, flags: OFlags, mode: Mode) -> std::result::Result<OwnedFd, Refused> {
    let dir = File::open(root).map_err(Refused::Root)?;
    let how = ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS | ResolveFlags::BENEATH;
    rustix::fs::openat2(&dir, relative, flags | OFlags::CLOEXEC, mode, how).map_err(Refused::Io)
}

/// `fd`'s status, failing unless it is a regular file.
fn regular(fd: &OwnedFd, relative: &str) -> Result<rustix::fs::Stat> {
    let stat = rustix::fs::fstat(fd).with_context(|| format!("stat {relative}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        bail!("refused: {relative} is not a regular file");
    }
    Ok(stat)
}

/// The first component of `relative` that is a symbolic link, for naming a
/// refusal. Each prefix is checked shortest first, so no link is followed.
fn first_link(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut at = PathBuf::new();
    for component in Path::new(relative).components() {
        at.push(component);
        if std::fs::symlink_metadata(root.join(&at)).is_ok_and(|m| m.file_type().is_symlink()) {
            return Some(at);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    /// A workspace beside a host directory holding a secret, with a link to
    /// the secret file (`hello.txt`) and a link to its directory (`sub`).
    struct Planted {
        top: PathBuf,
        workspace: PathBuf,
        secret: PathBuf,
    }

    const SECRET: &str = "host secret: do not print";

    impl Planted {
        fn new(name: &str) -> Self {
            let top = PathBuf::from(crate::DEFAULT_SCRATCH).join(format!("unit-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&top);
            let (workspace, outside) = (top.join("workspace"), top.join("outside"));
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            let secret = outside.join("secret.txt");
            std::fs::write(&secret, SECRET).unwrap();
            std::fs::write(workspace.join("plain.txt"), "plain\n").unwrap();
            symlink(&secret, workspace.join("hello.txt")).unwrap();
            symlink(&outside, workspace.join("sub")).unwrap();
            symlink(top.join("nowhere"), workspace.join("dangling")).unwrap();
            Self { top, workspace, secret }
        }

        fn secret_intact(&self) -> bool {
            std::fs::read_to_string(&self.secret).unwrap() == SECRET
        }
    }

    impl Drop for Planted {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.top);
        }
    }

    fn refusal(result: Result<impl std::fmt::Debug>) -> String {
        format!("{:#}", result.expect_err("a path through a symbolic link is refused"))
    }

    #[test]
    fn a_plain_file_is_read_written_and_found() {
        let p = Planted::new("ws-plain");
        assert_eq!(read(&p.workspace, "plain.txt").unwrap(), "plain\n");
        assert!(exists(&p.workspace, "plain.txt").unwrap());
        assert!(!exists(&p.workspace, "absent.txt").unwrap());
        assert!(!exists(&p.workspace, "absent/deeper.txt").unwrap());
        write_empty(&p.workspace, "new.txt").unwrap();
        assert_eq!(read(&p.workspace, "new.txt").unwrap(), "");
    }

    #[test]
    fn a_read_through_a_planted_link_is_refused_without_reading_it() {
        let p = Planted::new("ws-read");
        for (path, link) in [("hello.txt", "hello.txt"), ("sub/secret.txt", "sub")] {
            let error = refusal(read(&p.workspace, path));
            assert!(error.contains(&format!("{link} is a symbolic link")), "{path}: {error}");
            assert!(!error.contains(SECRET), "{path}: the refusal leaked the target: {error}");
        }
    }

    #[test]
    fn an_exists_check_through_a_planted_link_is_refused() {
        let p = Planted::new("ws-exists");
        for (path, link) in [("hello.txt", "hello.txt"), ("sub/secret.txt", "sub"), ("dangling", "dangling")] {
            let error = refusal(exists(&p.workspace, path));
            assert!(error.contains(&format!("{link} is a symbolic link")), "{path}: {error}");
        }
    }

    #[test]
    fn a_write_through_a_planted_link_is_refused_and_leaves_the_target() {
        let p = Planted::new("ws-write");
        for (path, link) in [("hello.txt", "hello.txt"), ("sub/secret.txt", "sub"), ("dangling", "dangling")] {
            let error = refusal(write_empty(&p.workspace, path));
            assert!(error.contains(&format!("{link} is a symbolic link")), "{path}: {error}");
            assert!(p.secret_intact(), "{path}: the write truncated the link's target");
        }
        assert!(!p.top.join("nowhere").exists(), "a write through a dangling link created its target");
    }

    #[test]
    fn a_fifo_is_refused_instead_of_blocking_the_runner() {
        let p = Planted::new("ws-fifo");
        let fifo = p.workspace.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        let error = refusal(read(&p.workspace, "pipe"));
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_file_past_the_read_limit_is_refused() {
        let p = Planted::new("ws-large");
        let big = std::fs::File::create(p.workspace.join("big")).unwrap();
        big.set_len(READ_LIMIT + 1).unwrap();
        let error = refusal(read(&p.workspace, "big"));
        assert!(error.contains("larger than"), "{error}");
    }
}
