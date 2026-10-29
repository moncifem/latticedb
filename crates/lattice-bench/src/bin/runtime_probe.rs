//! Opt-in Linux runtime comparison, using the real kernel and Io contract for every backend.
//! `runtime_probe --dir PATH --commits 2000 --read-passes 20 --output results.json`.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

#[cfg(target_os = "linux")]
#[path = "../runtime_spike/tokio_io.rs"]
mod tokio_io;

#[cfg(target_os = "linux")]
mod linux {
    use super::tokio_io::TokioIo;
    use lattice_core::{ChangeSet, Config, Kernel};
    use lattice_seams::{Io, IoError, OsRng, StdIo, SystemClock, UringIo};
    use serde_json::{Value, json};
    use std::cell::Cell;
    use std::path::Path;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    struct Timed<I> {
        inner: I,
        sync_time: Rc<Cell<Duration>>,
    }
    impl<I: Io> Io for Timed<I> {
        fn exists(&self, p: &str) -> bool {
            self.inner.exists(p)
        }
        fn len(&self, p: &str) -> Result<u64, IoError> {
            self.inner.len(p)
        }
        fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
            self.inner.read(p)
        }

        fn read_prefix(&self, p: &str, max: usize) -> Result<Vec<u8>, IoError> {
            self.inner.read_prefix(p, max)
        }

        fn append(&mut self, p: &str, b: &[u8]) -> Result<(), IoError> {
            self.inner.append(p, b)
        }
        fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
            self.inner.truncate(p, n)
        }
        fn sync(&mut self, p: &str) -> Result<(), IoError> {
            let start = Instant::now();
            let result = self.inner.sync(p);
            self.sync_time.set(self.sync_time.get() + start.elapsed());
            result
        }
        fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
            let start = Instant::now();
            let result = self.inner.sync_dir(p);
            self.sync_time.set(self.sync_time.get() + start.elapsed());
            result
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
            self.inner.unlock(p);
        }
    }

    fn backend<I: Io>(
        name: &str,
        root: &Path,
        commits: usize,
        read_passes: usize,
        make: impl Fn(&Path) -> Result<I, IoError>,
    ) -> Result<Vec<Value>, String> {
        let conformance = root.join(name).join("conformance");
        std::fs::create_dir_all(&conformance).map_err(|e| e.to_string())?;
        let mut io = make(&conformance).map_err(|e| format!("{name} setup: {e}"))?;
        lattice_seams::conformance::run(&mut io);
        drop(io);
        println!("{name}: Io conformance passed");
        let mut rows = Vec::new();
        for batch in [1usize, 4, 16, 64] {
            let dir = root.join(name).join(format!("b{batch}"));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let sync = Rc::new(Cell::new(Duration::ZERO));
            let io = Timed {
                inner: make(&dir).map_err(|e| e.to_string())?,
                sync_time: sync.clone(),
            };
            let (mut kernel, recovered) = Kernel::open(io, SystemClock, OsRng, &Config::default())
                .map_err(|e| e.to_string())?;
            if recovered.report.records != 0 {
                return Err("benchmark directory is not fresh".into());
            }
            sync.set(Duration::ZERO);
            let mut batch_latencies = Vec::new();
            let start = Instant::now();
            let mut done = 0usize;
            while done < commits {
                let sets: Vec<ChangeSet> = (0..batch.min(commits - done))
                    .map(|_| ChangeSet::new(kernel.new_id(), vec![7; 100]))
                    .collect();
                let count = sets.len();
                let batch_start = Instant::now();
                if let Some(error) = kernel.commit_batch(sets).into_iter().find_map(Result::err) {
                    return Err(error.to_string());
                }
                batch_latencies.push(batch_start.elapsed().as_secs_f64() * 1e6);
                done += count;
            }
            let elapsed = start.elapsed().as_secs_f64();
            let fsync = sync.get().as_secs_f64();
            let read_start = Instant::now();
            let mut read_records = 0usize;
            for _ in 0..read_passes {
                let (records, _) = kernel.read_from(1, commits).map_err(|e| e.to_string())?;
                if records.len() != commits {
                    return Err(format!(
                        "{name}: scan returned {} records, expected {commits}",
                        records.len()
                    ));
                }
                read_records += std::hint::black_box(records).len();
            }
            let read_seconds = read_start.elapsed().as_secs_f64();
            drop(kernel);
            let (kernel, _) = Kernel::open(
                make(&dir).map_err(|e| e.to_string())?,
                SystemClock,
                OsRng,
                &Config::default(),
            )
            .map_err(|e| e.to_string())?;
            let mut recovered_changes = 0usize;
            for record in &kernel.read_from(1, commits).map_err(|e| e.to_string())?.0 {
                if lattice_core::record::RecordType::from_u8(record.rtype)
                    != Some(lattice_core::record::RecordType::ChangeSet)
                {
                    return Err(format!("{name}: unexpected recovered record"));
                }
                let change =
                    ChangeSet::decode(&record.payload).map_err(|e| format!("{name}: {e}"))?;
                if change.payload != vec![7; 100] {
                    return Err(format!("{name}: recovered payload differs"));
                }
                recovered_changes += 1;
            }
            if recovered_changes != commits {
                return Err(format!(
                    "{name}: recovered {recovered_changes}, expected {commits}"
                ));
            }
            drop(kernel);
            batch_latencies.sort_by(f64::total_cmp);
            let percentile = |p: usize| -> f64 {
                let idx = (batch_latencies.len().saturating_sub(1) * p) / 100;
                batch_latencies.get(idx).copied().unwrap_or(0.0)
            };
            println!(
                "{name:>8} batch={batch:>2} commits/s={:.0} us/commit={:.1} fsync={:.1}% read-records/s={:.0}",
                commits as f64 / elapsed,
                elapsed * 1e6 / commits as f64,
                100.0 * fsync / elapsed,
                read_records as f64 / read_seconds
            );
            rows.push(json!({"backend":name,"batch":batch,"commits":commits,"payload_bytes":100,"commit_seconds":elapsed,"fsync_seconds":fsync,"fsync_share":fsync/elapsed,"commits_per_second":commits as f64/elapsed,"us_per_commit":elapsed*1e6/commits as f64,"batch_ack_p50_us":percentile(50),"batch_ack_p99_us":percentile(99),"read_passes":read_passes,"read_records":read_records,"read_seconds":read_seconds,"read_records_per_second":read_records as f64/read_seconds,"conformance":"passed","recovery":"passed"}));
        }
        Ok(rows)
    }
    fn arg(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }
    pub(super) fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let commits = arg(&args, "--commits")
            .unwrap_or_else(|| "2000".into())
            .parse::<usize>()
            .map_err(|e| e.to_string())?;
        let passes = arg(&args, "--read-passes")
            .unwrap_or_else(|| "20".into())
            .parse::<usize>()
            .map_err(|e| e.to_string())?;
        if commits == 0 || passes == 0 {
            return Err("commits and read-passes must be positive".into());
        }
        let base = arg(&args, "--dir").ok_or("--dir is required and must not already exist")?;
        let root = Path::new(&base);
        std::fs::create_dir(root).map_err(|e| format!("fresh benchmark root {base}: {e}"))?;
        let mut rows = Vec::new();
        let repetitions = 3usize;
        let mut error = None;
        for repetition in 0..repetitions {
            let dir = root.join(format!("r{repetition}"));
            std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
            let mut round = backend("portable", &dir, commits, passes, |p| Ok(StdIo::new(p)))?;
            round.extend(backend("tokio", &dir, commits, passes, |p| {
                TokioIo::new(p)
            })?);
            match backend("uring", &dir, commits, passes, |p| UringIo::new(p)) {
                Ok(data) => round.extend(data),
                Err(why) => error = Some(why),
            }
            for row in &mut round {
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("repetition".into(), json!(repetition));
                }
            }
            rows.extend(round);
            if error.is_some() {
                break;
            }
        }
        let status = if error.is_some() {
            "incomplete"
        } else {
            "complete"
        };
        let report = json!({
            "status": status,
            "error": error,
            "revision": std::env::var("GITHUB_SHA").ok(),
            "platform": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()
                .map(|s| s.trim().to_string()),
            "requested_repetitions": repetitions,
            "rows": rows,
            "notes": [
                "Shared hosted-runner disk; these are measurements, not production NVMe evidence.",
                "The existing synchronous kernel is unchanged. Each adapter waits for real I/O completion.",
                "Tokio filesystem operations use its blocking pool; append flushes before returning.",
                "io_uring metadata/locks remain portable; read/write/fsync use the ring, without fallback.",
                "One request is in flight to preserve Io; this is not a redesigned async pipeline.",
                "Read timings include WAL decoding and allocations, not a syscall-only profile."
            ]
        });
        let output = arg(&args, "--output").unwrap_or_else(|| "runtime-probe.json".into());
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        match linux::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("runtime comparison incomplete: {e}");
                std::process::ExitCode::from(2)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("runtime_probe requires Linux; no adapter was run");
        std::process::ExitCode::from(2)
    }
}
