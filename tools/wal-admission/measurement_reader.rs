//! Host-only template compiled against each actual source revision by measure.py.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout
)]
use lattice_core::Log;
use lattice_seams::{Io, IoError, StdIo};
use std::{cell::Cell, rc::Rc, time::Instant};

#[derive(Default)]
struct Counts {
    full_reads: Cell<usize>,
    full_bytes: Cell<usize>,
    prefix_reads: Cell<usize>,
    prefix_bytes: Cell<usize>,
    max_request: Cell<usize>,
    max_return: Cell<usize>,
    max_capacity: Cell<usize>,
}
struct CountIo {
    inner: StdIo,
    counts: Rc<Counts>,
}
impl Io for CountIo {
    fn exists(&self, p: &str) -> bool {
        self.inner.exists(p)
    }
    fn len(&self, p: &str) -> Result<u64, IoError> {
        self.inner.len(p)
    }
    fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
        let bytes = self.inner.read(p)?;
        self.counts.full_reads.set(self.counts.full_reads.get() + 1);
        self.counts
            .full_bytes
            .set(self.counts.full_bytes.get() + bytes.len());
        Ok(bytes)
    }
    // PREFIX_METHOD
    fn append(&mut self, p: &str, b: &[u8]) -> Result<(), IoError> {
        self.inner.append(p, b)
    }
    fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
        self.inner.truncate(p, n)
    }
    fn sync(&mut self, p: &str) -> Result<(), IoError> {
        self.inner.sync(p)
    }
    fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
        self.inner.sync_dir(p)
    }
    fn list(&self, p: &str) -> Result<Vec<String>, IoError> {
        self.inner.list(p)
    }
    fn remove(&mut self, p: &str) -> Result<(), IoError> {
        self.inner.remove(p)
    }
    fn create_dir_all(&mut self, p: &str) -> Result<(), IoError> {
        self.inner.create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> Result<(), IoError> {
        self.inner.lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.inner.unlock(p)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).ok_or("missing root")?;
    let counts = Rc::new(Counts::default());
    let io = CountIo {
        inner: StdIo::new(root),
        counts: counts.clone(),
    };
    let mut records = 0_u64;
    let mut bytes = 0_usize;
    let mut last_lsn = 0_u64;
    let start = Instant::now();
    // OPEN_LOG
    let elapsed = start.elapsed().as_nanos();
    if report.records != records || report.truncated_bytes != 0 || last_lsn != records {
        return Err("recovery count, continuity or repair differs from clean dataset".into());
    }
    println!(
        "{{\"open_ns\":{elapsed},\"records\":{records},\"payload_bytes\":{bytes},\"segments\":{},\"full_reads\":{},\"full_bytes\":{},\"prefix_reads\":{},\"prefix_bytes\":{},\"max_prefix_request\":{},\"max_prefix_return\":{},\"max_prefix_capacity\":{}}}",
        report.segments,
        counts.full_reads.get(),
        counts.full_bytes.get(),
        counts.prefix_reads.get(),
        counts.prefix_bytes.get(),
        counts.max_request.get(),
        counts.max_return.get(),
        counts.max_capacity.get()
    );
    drop(log);
    Ok(())
}
