//! Tokio filesystem adapter used only by the Linux runtime comparison.
use lattice_seams::{Io, IoError, StdIo};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::runtime::{Builder, Runtime};

fn map(path: &str, error: std::io::Error) -> IoError {
    match error.kind() {
        std::io::ErrorKind::NotFound => IoError::NotFound(path.into()),
        std::io::ErrorKind::StorageFull => IoError::NoSpace,
        _ => IoError::Other(format!("{path}: {error}")),
    }
}

pub(super) struct TokioIo {
    root: PathBuf,
    metadata: StdIo,
    runtime: Runtime,
}
impl TokioIo {
    pub(super) fn new(root: impl Into<PathBuf>) -> Result<Self, IoError> {
        let root = root.into();
        let runtime = Builder::new_current_thread()
            .build()
            .map_err(|e| map("tokio runtime", e))?;
        Ok(Self {
            metadata: StdIo::new(&root),
            root,
            runtime,
        })
    }
}
impl Io for TokioIo {
    fn exists(&self, path: &str) -> bool {
        self.metadata.exists(path)
    }
    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.metadata.len(path)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        self.runtime
            .block_on(tokio::fs::read(self.root.join(path)))
            .map_err(|e| map(path, e))
    }
    fn read_prefix(&self, path: &str, max: usize) -> Result<Vec<u8>, IoError> {
        self.metadata.read_prefix(path, max)
    }
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.runtime
            .block_on(async {
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.root.join(path))
                    .await?;
                f.write_all(data).await?;
                // Tokio writes may remain in its blocking task after write_all returns. Flush waits for
                // completion, preserving Io's immediate visibility contract before append returns.
                f.flush().await
            })
            .map_err(|e| map(path, e))
    }
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.runtime
            .block_on(async {
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(self.root.join(path))
                    .await?
                    .set_len(len)
                    .await
            })
            .map_err(|e| map(path, e))
    }
    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        self.runtime
            .block_on(async {
                tokio::fs::File::open(self.root.join(path))
                    .await?
                    .sync_all()
                    .await
            })
            .map_err(|e| map(path, e))
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
