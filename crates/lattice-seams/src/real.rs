//! Real implementations of the seams.

use crate::{Clock, Io, IoError, Rng, Spawner};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

/// The system wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Operating system randomness.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRng;

impl Rng for OsRng {
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        // Ids and keys depend on this. Retrying covers transient failures; a persistent failure
        // means the platform cannot give us entropy, and handing out predictable or repeated values
        // would silently turn distinct writes into duplicates. Stop the process instead.
        for _ in 0..3 {
            if getrandom::fill(&mut b).is_ok() {
                return u64::from_le_bytes(b);
            }
        }
        eprintln!("fatal: the operating system cannot provide random bytes");
        std::process::abort()
    }
}

/// Runs each task on its own OS thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadSpawner;

impl Spawner for ThreadSpawner {
    fn spawn(&mut self, task: Box<dyn FnOnce() + Send + 'static>) {
        std::thread::spawn(task);
    }
}

fn map(path: &str, e: std::io::Error) -> IoError {
    match e.kind() {
        std::io::ErrorKind::NotFound => IoError::NotFound(path.to_string()),
        std::io::ErrorKind::StorageFull => IoError::NoSpace,
        _ => IoError::Other(format!("{path}: {e}")),
    }
}

/// Filesystem storage rooted at a directory.
#[derive(Debug)]
pub struct StdIo {
    root: PathBuf,
    locks: BTreeMap<String, fs::File>,
}

impl StdIo {
    /// Creates storage rooted at `root`, which must exist.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: BTreeMap::new(),
        }
    }

    fn full(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }
}

impl Io for StdIo {
    fn exists(&self, path: &str) -> bool {
        self.full(path).exists()
    }

    fn len(&self, path: &str) -> Result<u64, IoError> {
        fs::metadata(self.full(path))
            .map(|m| m.len())
            .map_err(|e| map(path, e))
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        fs::read(self.full(path)).map_err(|e| map(path, e))
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.full(path))
            .map_err(|e| map(path, e))?;
        f.write_all(data).map_err(|e| map(path, e))
    }

    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(self.full(path))
            .map_err(|e| map(path, e))?;
        f.set_len(len).map_err(|e| map(path, e))
    }

    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        let f = fs::File::open(self.full(path)).map_err(|e| map(path, e))?;
        f.sync_all().map_err(|e| map(path, e))
    }

    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError> {
        let f = fs::File::open(self.full(dir)).map_err(|e| map(dir, e))?;
        f.sync_all().map_err(|e| map(dir, e))
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, IoError> {
        let mut names: Vec<String> = fs::read_dir(self.full(dir))
            .map_err(|e| map(dir, e))?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        fs::remove_file(self.full(path)).map_err(|e| map(path, e))
    }

    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError> {
        fs::create_dir_all(self.full(dir)).map_err(|e| map(dir, e))
    }

    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        if self.locks.contains_key(path) {
            return Err(IoError::Other(format!(
                "{path} is already locked by this handle"
            )));
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.full(path))
            .map_err(|e| map(path, e))?;
        match f.try_lock() {
            Ok(()) => {
                self.locks.insert(path.to_string(), f);
                Ok(())
            }
            Err(fs::TryLockError::WouldBlock) => Err(IoError::Other(format!(
                "{path} is locked by another writer"
            ))),
            Err(fs::TryLockError::Error(e)) => Err(map(path, e)),
        }
    }

    fn unlock(&mut self, path: &str) {
        // Dropping the file releases the lock.
        self.locks.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_io_satisfies_the_contract() {
        let dir = std::env::temp_dir().join(format!("lattice-seams-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut io = StdIo::new(&dir);
        crate::conformance::run(&mut io);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn os_rng_is_not_constant() {
        let mut r = OsRng;
        assert_ne!(r.next_u64(), r.next_u64());
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }

    #[test]
    fn the_spawner_runs_the_task() {
        let (tx, rx) = std::sync::mpsc::channel();
        ThreadSpawner.spawn(Box::new(move || {
            tx.send(7u8).unwrap();
        }));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(7));
    }

    #[test]
    fn missing_paths_are_reported_by_exists_sync_and_sync_dir() {
        let dir = std::env::temp_dir().join(format!("lattice-seams-miss-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut io = StdIo::new(&dir);
        assert!(!io.exists("nope"));
        assert_eq!(io.sync("nope"), Err(IoError::NotFound("nope".into())));
        assert_eq!(io.sync_dir("nodir"), Err(IoError::NotFound("nodir".into())));
        io.append("here", b"x").unwrap();
        assert!(io.exists("here"));
        assert!(io.sync("here").is_ok());
        assert!(io.sync_dir("").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }
}
