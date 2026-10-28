//! Opt-in Linux io_uring adapter. Metadata and exclusive locks remain the portable implementation;
//! data reads, writes and fsyncs go through the ring. No core/kernel code knows this adapter exists.

use crate::{Io, IoError, StdIo};
use lattice_sys::uring::Ring;
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

fn map(path: &str, error: std::io::Error) -> IoError {
    match error.kind() {
        std::io::ErrorKind::NotFound => IoError::NotFound(path.into()),
        std::io::ErrorKind::StorageFull => IoError::NoSpace,
        _ => IoError::Other(format!("{path}: {error}")),
    }
}

/// Experimental synchronous Io adapter using real Linux io_uring requests.
pub struct UringIo {
    root: PathBuf,
    metadata: StdIo,
    ring: RefCell<Ring>,
}

impl UringIo {
    /// Initializes an io_uring instance. A denied/unsupported ring is an error, never a fallback.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, IoError> {
        let root = root.into();
        Ok(Self {
            metadata: StdIo::new(&root),
            root,
            ring: RefCell::new(Ring::new().map_err(|e| map("io_uring_setup", e))?),
        })
    }
}

impl Io for UringIo {
    fn exists(&self, path: &str) -> bool {
        self.metadata.exists(path)
    }
    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.metadata.len(path)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        let file = fs::File::open(self.root.join(path)).map_err(|e| map(path, e))?;
        let len = usize::try_from(file.metadata().map_err(|e| map(path, e))?.len())
            .map_err(|_| IoError::Other("file exceeds address space".into()))?;
        let mut bytes = vec![0; len];
        let mut done = 0usize;
        while done < len {
            let rest = bytes
                .get_mut(done..)
                .ok_or_else(|| IoError::Other("invalid read offset".into()))?;
            let request = rest.len().min(u32::MAX as usize);
            let part = rest
                .get_mut(..request)
                .ok_or_else(|| IoError::Other("invalid read length".into()))?;
            let n = self
                .ring
                .borrow_mut()
                .read(&file, part, done as u64)
                .map_err(|e| map(path, e))?;
            if n == 0 {
                break;
            }
            done += n;
        }
        bytes.truncate(done);
        Ok(bytes)
    }
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(path))
            .map_err(|e| map(path, e))?;
        let base = file.metadata().map_err(|e| map(path, e))?.len();
        let mut done = 0usize;
        while done < data.len() {
            let rest = data
                .get(done..)
                .ok_or_else(|| IoError::Other("invalid write offset".into()))?;
            let part = rest
                .get(..rest.len().min(u32::MAX as usize))
                .ok_or_else(|| IoError::Other("invalid write length".into()))?;
            let offset = base
                .checked_add(done as u64)
                .ok_or_else(|| IoError::Other("file offset overflow".into()))?;
            let n = self
                .ring
                .borrow_mut()
                .write(&file, part, offset)
                .map_err(|e| map(path, e))?;
            if n == 0 {
                return Err(IoError::Other("io_uring returned a zero-byte write".into()));
            }
            done += n;
        }
        Ok(())
    }
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.metadata.truncate(path, len)
    }
    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        let file = fs::File::open(self.root.join(path)).map_err(|e| map(path, e))?;
        self.ring.borrow_mut().sync(&file).map_err(|e| map(path, e))
    }
    fn sync_dir(&mut self, path: &str) -> Result<(), IoError> {
        self.sync(path)
    }
    fn list(&self, path: &str) -> Result<Vec<String>, IoError> {
        self.metadata.list(path)
    }
    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        self.metadata.remove(path)
    }
    fn create_dir_all(&mut self, path: &str) -> Result<(), IoError> {
        self.metadata.create_dir_all(path)
    }
    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        self.metadata.lock(path)
    }
    fn unlock(&mut self, path: &str) {
        self.metadata.unlock(path);
    }
}
