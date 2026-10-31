//! Owner-only files shared by the credential loader and the credential tool (`lattice account`).
//!
//! Everything here is Unix-only for now: on other platforms each function refuses, because a
//! credential file must not be read or written without an owner-only check (#577).
//!
//! - [`replace_private`] atomically replaces a file (mode 0600).
//! - [`create_private_new`] atomically creates a file and refuses one that exists.
//! - [`read_private`] reads a bounded file that group and others cannot access, in a directory they
//!   cannot write to.
//!
//! A temporary 0600 file in the same directory is written and synced first, so a crash leaves the
//! old file or the new one, never a mix, and no byte is ever readable by another user.

use std::io;
use std::path::Path;
use zeroize::Zeroizing;

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "credential files need an owner-only ACL check that this platform does not have yet",
    )
}

#[cfg(unix)]
pub(crate) fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// A directory that others can write to lets them swap the file after the check.
#[cfg(unix)]
pub(crate) fn require_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(parent_of(path))?;
    if !metadata.is_dir() {
        return Err(io::Error::other("credential directory is not a directory"));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::other(
            "credential directory is writable by group or others (use mode 0700)",
        ));
    }
    Ok(())
}

/// Reads at most `limit` bytes of the file at `path`.
///
/// The file must be a regular file with no group or other access, in a directory that group and
/// others cannot write to. A FIFO or symlink at the path is refused without blocking. A file over
/// `limit` bytes fails with `InvalidData`; the other refusals are `Other` (or the open error).
/// Off Unix every call refuses with `Unsupported`.
pub fn read_private(path: &Path, limit: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    #[cfg(not(unix))]
    {
        let _ = (path, limit);
        Err(unsupported())
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        require_private_directory(path)?;
        // O_NONBLOCK so a FIFO at the path cannot block the open forever, O_NOFOLLOW so a symlink
        // is refused; the regular-file check then runs on the open descriptor.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)?;
        crate::require_private_file(&file)?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential file is too large",
            ));
        }
        Ok(bytes)
    }
}

/// Takes an exclusive advisory lock on `DIR/.lock` and returns the open file; the lock is released
/// when the file is dropped (or the process ends). It blocks while another process holds it.
///
/// A credential tool holds it across every read-modify-write of the directory so that two runs
/// cannot write back stale copies over each other (a token created during a revocation must not
/// undo it). The directory must be one that group and others cannot write to; the lock file is
/// created 0600 and a symlink in its place is refused.
pub fn lock_dir(dir: &Path) -> io::Result<std::fs::File> {
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(unsupported())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join(".lock");
        require_private_directory(&path)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("lock path is not a regular file"));
        }
        // Planted defect: the lock is never taken.
        if !lattice_core::bugs::active(lattice_core::bugs::Bug::AccountNoLock) {
            file.lock()?;
        }
        Ok(file)
    }
}

/// Atomically replaces the file at `path` with `bytes` (mode 0600). The caller serializes writers.
pub fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private(path, bytes, false)
}

/// Atomically creates the file at `path` with `bytes` (mode 0600). A file that already exists is
/// an error (`AlreadyExists`) and is left untouched, so provisioning twice cannot destroy a secret.
pub fn create_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private(path, bytes, true)
}

fn write_private(path: &Path, bytes: &[u8], refuse_existing: bool) -> io::Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (path, bytes, refuse_existing);
        Err(unsupported())
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        require_private_directory(path)?;
        let dir = parent_of(path);
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("credential path has no file name"))?;
        let mut temporary = name.to_os_string();
        // Unique per write; `create_new` below refuses a name that already exists.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        temporary.push(format!(".tmp{}-{serial}", std::process::id()));
        let temporary = dir.join(temporary);
        // Only a file this call created is ever removed.
        let mut created = false;
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            created = true;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // `hard_link` fails if the target exists, which makes "create only" atomic; `rename`
            // replaces. Planted defect: the create-only path replaces too.
            let replace = !refuse_existing
                || lattice_core::bugs::active(lattice_core::bugs::Bug::AccountFileOverwritten);
            if replace {
                std::fs::rename(&temporary, path)?;
                created = false;
            } else {
                std::fs::hard_link(&temporary, path)?;
            }
            std::fs::File::open(dir)?.sync_all()
        })();
        if created {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use lattice_core::bugs::{self, Bug};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn directory(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("latticed-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn files_are_created_0600_and_refuse_overwrite() {
        let dir = directory("create");
        let path = dir.join("secret");
        create_private_new(&path, b"first").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let error = create_private_new(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        // The refused write leaves no temporary file behind.
        assert_eq!(names(&dir), ["secret"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_planted_overwrite_is_caught_by_the_refusal_test() {
        let dir = directory("planted");
        let path = dir.join("secret");
        create_private_new(&path, b"first").unwrap();
        bugs::enable(Bug::AccountFileOverwritten);
        let outcome = create_private_new(&path, b"second");
        bugs::clear();
        // The defect replaced the file: exactly what the previous test forbids.
        assert!(outcome.is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(names(&dir), ["secret"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replace_gives_the_new_file_mode_0600_and_reads_back() {
        let dir = directory("replace");
        let path = dir.join("secret");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        replace_private(&path, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(&**read_private(&path, 16).unwrap(), b"new");
        assert!(read_private(&path, 2).is_err());
        assert_eq!(names(&dir), ["secret"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_and_writes_refuse_an_open_directory_and_a_broad_file() {
        let dir = directory("modes");
        let path = dir.join("secret");
        create_private_new(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read_private(&path, 8).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(read_private(&path, 8).is_err());
        assert!(replace_private(&path, b"y").is_err());
        assert!(create_private_new(&dir.join("other"), b"y").is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
