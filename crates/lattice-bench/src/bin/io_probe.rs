//! Measures the real commit path on a real disk: how long commits take at several batch sizes and
//! how much of that time is spent inside fsync. This is the evidence the runtime decision (ADR 0011)
//! rests on. Usage: `io-probe [--dir PATH] [--commits N]`.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::disallowed_methods)]

use lattice_core::changeset::ChangeSet;
use lattice_core::{Config, Kernel};
use lattice_seams::{Io, IoError, OsRng, StdIo, SystemClock};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Delegates to a real `Io` and adds up the time spent making data durable.
struct Timed {
    inner: StdIo,
    sync_time: Rc<Cell<Duration>>,
}

impl Io for Timed {
    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }
    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.inner.len(path)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        self.inner.read(path)
    }
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.inner.append(path, data)
    }
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.inner.truncate(path, len)
    }
    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        let start = Instant::now();
        let out = self.inner.sync(path);
        self.sync_time.set(self.sync_time.get() + start.elapsed());
        out
    }
    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError> {
        let start = Instant::now();
        let out = self.inner.sync_dir(dir);
        self.sync_time.set(self.sync_time.get() + start.elapsed());
        out
    }
    fn list(&self, dir: &str) -> Result<Vec<String>, IoError> {
        self.inner.list(dir)
    }
    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        self.inner.remove(path)
    }
    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError> {
        self.inner.create_dir_all(dir)
    }
    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        self.inner.lock(path)
    }
    fn unlock(&mut self, path: &str) {
        self.inner.unlock(path);
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let commits: usize = arg(&args, "--commits")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let base = arg(&args, "--dir").unwrap_or_else(|| {
        std::env::temp_dir()
            .join(format!("lattice-io-probe-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    });
    println!("commit path on a real disk ({commits} commits of 100 bytes per row), dir {base}");
    println!(
        "{:>6} {:>12} {:>14} {:>12}",
        "batch", "commits/s", "us per commit", "fsync share"
    );
    for batch in [1usize, 4, 16, 64] {
        let dir = format!("{base}/b{batch}");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("cannot create {dir}: {e}");
            return std::process::ExitCode::from(2);
        }
        let sync_time = Rc::new(Cell::new(Duration::ZERO));
        let io = Timed {
            inner: StdIo::new(&dir),
            sync_time: sync_time.clone(),
        };
        let opened = Kernel::open(io, SystemClock, OsRng, &Config::default());
        let Ok((mut kernel, _)) = opened else {
            eprintln!("cannot open a kernel in {dir}");
            return std::process::ExitCode::from(2);
        };
        sync_time.set(Duration::ZERO);
        let start = Instant::now();
        let mut done = 0;
        while done < commits {
            let sets: Vec<ChangeSet> = (0..batch.min(commits - done))
                .map(|_| ChangeSet::new(kernel.new_id(), vec![7u8; 100]))
                .collect();
            let n = sets.len();
            if kernel.commit_batch(sets).iter().any(Result::is_err) {
                eprintln!("a commit failed");
                return std::process::ExitCode::from(2);
            }
            done += n;
        }
        let total = start.elapsed();
        println!(
            "{:>6} {:>12.0} {:>14.1} {:>11.0}%",
            batch,
            commits as f64 / total.as_secs_f64(),
            total.as_secs_f64() * 1e6 / commits as f64,
            100.0 * sync_time.get().as_secs_f64() / total.as_secs_f64()
        );
    }
    let _ = std::fs::remove_dir_all(&base);
    std::process::ExitCode::SUCCESS
}
