//! NS1 with real OS processes/files: kill a writer, recover acknowledged complete row batches.
//! Process termination preserves the OS page cache. This is not a power-loss or soak test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_core::changeset::ChangeSet;
use lattice_core::record::RecordType;
use lattice_core::{Config, Kernel};
use lattice_engines::index::IndexDef;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::rowops::{self, RowWrite};
use lattice_engines::store::RowStore;
use lattice_seams::{OsRng, Rng, StdIo, SystemClock};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

type Store = RowStore<StdIo, SystemClock, OsRng>;
type Rows = BTreeMap<(u32, Vec<u8>), Vec<u8>>;
const CHILD_ROOT: &str = "LATTICE_NS1_CHILD_ROOT";
const TIMEOUT: Duration = Duration::from_secs(20);
const MARKERS: u32 = 100;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 8_192,
        node: 1,
    }
}

fn open(root: &Path) -> Store {
    RowStore::open_with_indexes(
        StdIo::new(root),
        SystemClock,
        OsRng,
        &config(),
        (0..3)
            .map(|table| IndexDef {
                id: table,
                table,
                extract: |v| v.first().map(|b| vec![*b]),
            })
            .collect(),
    )
    .unwrap()
}

/// Parent and child derive the submitted envelope from a sequence number, without reading rows.
fn envelope(n: u64) -> Vec<RowWrite> {
    let mut writes = BTreeMap::new();
    for table in 0..3u32 {
        let key = vec![(n % 3) as u8];
        let value = if n % 4 == 3 {
            None
        } else {
            let mut bytes = vec![((n + u64::from(table)) % 7) as u8; 1_024];
            bytes
                .get_mut(1..9)
                .unwrap()
                .copy_from_slice(&n.to_le_bytes());
            Some(bytes)
        };
        writes.insert((table, key), value);
    }
    writes.insert(
        (MARKERS, n.to_le_bytes().to_vec()),
        Some(n.to_le_bytes().to_vec()),
    );
    writes.into_iter().collect()
}

#[test]
#[allow(clippy::disallowed_methods)] // Explicit parent-to-child test configuration, not core ambient config.
fn ns1_subprocess_writer() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let count: u64 = std::env::var("LATTICE_NS1_CHILD_COUNT")
        .unwrap()
        .parse()
        .unwrap();
    let mut store = open(Path::new(&root));
    let mut stdout = std::io::stdout().lock();
    for n in 0..count {
        // Submission must reach the parent pipe before any record can reach the WAL.
        writeln!(stdout, "NS1 SUBMIT {n}").unwrap();
        stdout.flush().unwrap();
        let mut tx = store.begin();
        for (key, value) in envelope(n) {
            match value {
                Some(v) => tx.put(key, v),
                None => tx.delete(key),
            }
        }
        let lsn = store.commit(tx).unwrap();
        // No receipt is emitted until the durable commit call has returned.
        writeln!(stdout, "NS1 ACK {n} {lsn}").unwrap();
        stdout.flush().unwrap();
    }
}

struct Directory(PathBuf);
impl Directory {
    fn new(seed: u64) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lattice-ns1-{}-{seed}-{}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Writer {
    child: Child,
    readers: Vec<thread::JoinHandle<()>>,
}
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[derive(Debug)]
enum Event {
    Submit(u64),
    Ack(u64, u64),
    Bad(String),
}

#[derive(Debug, Default)]
struct Observed {
    submitted: Vec<u64>,
    acked: BTreeMap<u64, u64>,
}
impl Observed {
    fn accept(&mut self, event: Event) {
        match event {
            Event::Submit(n) => {
                assert_eq!(
                    n,
                    self.submitted.len() as u64,
                    "non-prefix submission: {self:?}"
                );
                self.submitted.push(n);
            }
            Event::Ack(n, lsn) => {
                assert_eq!(self.submitted.last(), Some(&n), "receipt before submission");
                assert!(self.acked.insert(n, lsn).is_none(), "duplicate receipt");
            }
            Event::Bad(line) => panic!("invalid child protocol: {line}"),
        }
    }
}

#[allow(clippy::disallowed_methods)] // This OS-process test deliberately uses real children, pipe threads and deadlines.
fn run_child(root: &Path, schedule: Option<u64>) -> Observed {
    let count = if schedule.is_some() { 4_096 } else { 16 };
    let mut child = Writer {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ns1_subprocess_writer",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ROOT, root)
            .env("LATTICE_NS1_CHILD_COUNT", count.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        readers: Vec::new(),
    };
    let stdout = child.child.stdout.take().unwrap();
    let stderr = child.child.stderr.take().unwrap();
    let (send, recv) = mpsc::channel();
    child.readers.push(thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            if input.read_line(&mut line).unwrap() == 0 {
                break;
            }
            // A killed process may leave part of an ACK line in the pipe: it is unobserved.
            if !line.ends_with('\n') {
                break;
            }
            let Some(start) = line.find("NS1 ") else {
                continue;
            };
            let parts = line
                .get(start..)
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>();
            let event = match parts.as_slice() {
                ["NS1", "SUBMIT", n] => n.parse().map(Event::Submit),
                ["NS1", "ACK", n, lsn] => n
                    .parse()
                    .and_then(|n| lsn.parse().map(|lsn| Event::Ack(n, lsn))),
                _ => {
                    let _ = send.send(Event::Bad(line));
                    continue;
                }
            }
            .unwrap_or(Event::Bad(line));
            if send.send(event).is_err() {
                break;
            }
        }
    }));
    let (stderr_send, stderr_recv) = mpsc::channel();
    child.readers.push(thread::spawn(move || {
        let output = std::io::read_to_string(stderr).unwrap();
        let _ = stderr_send.send(output);
    }));
    let deadline = Instant::now() + TIMEOUT;
    let mut observed = Observed::default();
    let mut schedule_rng = lattice_sim::SeededRng::new(schedule.unwrap_or(0));
    let target = schedule.map(|_| 4 + schedule_rng.below(8) as usize);
    let delay_us = schedule_rng.below(2_000);
    if let Some(target) = target {
        while observed.acked.len() < target {
            observed.accept(
                recv.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("writer did not acknowledge the requested prefix before timeout"),
            );
        }
        // Seeded schedules are reproducible; exact OS interruption boundaries are not.
        thread::sleep(Duration::from_micros(delay_us));
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "writer ended before kill"
        );
        child.child.kill().unwrap();
    }
    let status = loop {
        if let Some(status) = child.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "child wait exceeded timeout");
        thread::sleep(Duration::from_millis(1));
    };
    for reader in child.readers.drain(..) {
        reader.join().unwrap();
    }
    let stderr = stderr_recv.recv().unwrap();
    for event in recv {
        observed.accept(event);
    }
    if schedule.is_some() {
        assert!(
            !status.success(),
            "kill unexpectedly reported success: {stderr}"
        );
        assert!(observed.acked.len() >= target.unwrap());
        assert!(
            observed.submitted.len() < count,
            "writer exhausted its burst before interruption"
        );
    } else {
        assert!(status.success(), "clean writer failed: {stderr}");
        assert_eq!(observed.acked.len(), count);
        assert_eq!(observed.submitted.len(), count);
    }
    observed
}

fn compare_rows(store: &Store, snap: Snapshot, expected: &Rows) {
    for table in [0, 1, 2, MARKERS] {
        let model = expected
            .iter()
            .filter(|((t, _), _)| *t == table)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect::<BTreeMap<_, _>>();
        let got = store
            .scan_at(snap, table, ..)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(got, model, "table={table}, snapshot={snap:?}");
        for key in 0..4u8 {
            assert_eq!(
                store.get_at(snap, table, &[key]).unwrap(),
                expected.get(&(table, vec![key]))
            );
        }
        if table != MARKERS {
            for value in 0..8u8 {
                let pks = model
                    .iter()
                    .filter(|(_, v)| v.first() == Some(&value))
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>();
                assert_eq!(
                    store.lookup_at(snap, table, &[value]).unwrap(),
                    pks,
                    "index={table} probe={value} snapshot={snap:?}"
                );
            }
        }
    }
    store.check_indexes_at(snap).unwrap();
}

fn verify(root: &Path, observed: &Observed) -> usize {
    let (kernel, _) = Kernel::open(StdIo::new(root), SystemClock, OsRng, &config()).unwrap();
    assert!(kernel.scrub().unwrap().issues.is_empty());
    let mut history = vec![(Snapshot { as_of: 0 }, Rows::new())];
    let mut rows = Rows::new();
    let mut ids = BTreeSet::new();
    let mut recovered = BTreeMap::new();
    for record in kernel.read_from(1, usize::MAX).unwrap().0 {
        assert_eq!(
            RecordType::from_u8(record.rtype),
            Some(RecordType::ChangeSet)
        );
        let cs = ChangeSet::decode(&record.payload).unwrap();
        assert!(ids.insert(cs.id), "duplicate recovered id");
        let writes = rowops::decode(&cs.payload).unwrap();
        let n = recovered.len() as u64;
        assert!(
            observed.submitted.contains(&n),
            "unknown recovered transaction {n}"
        );
        assert_eq!(
            writes,
            envelope(n),
            "partial/reordered/unknown envelope {n}"
        );
        assert!(recovered.insert(n, record.lsn).is_none());
        for (key, value) in envelope(n) {
            match value {
                Some(value) => {
                    rows.insert(key, value);
                }
                None => {
                    rows.remove(&key);
                }
            }
        }
        history.push((Snapshot { as_of: record.lsn }, rows.clone()));
    }
    for (n, lsn) in &observed.acked {
        assert_eq!(
            recovered.get(n),
            Some(lsn),
            "acknowledged transaction {n} missing or at wrong LSN"
        );
    }
    assert!(recovered.len() <= observed.submitted.len());
    drop(kernel);
    // First row recovery and independent full map/snapshot/index comparisons.
    let store = open(root);
    for (snap, model) in &history {
        compare_rows(&store, *snap, model);
    }
    compare_rows(&store, store.snapshot(), &rows);
    drop(store);
    // A second terminal recovery must preserve everything the first recovery adopted.
    let store = open(root);
    compare_rows(&store, store.snapshot(), &rows);
    assert!(
        lattice_core::scrub::scrub(&StdIo::new(root), "wal")
            .unwrap()
            .issues
            .is_empty()
    );
    recovered.len()
}

fn clean_controls_recover_every_receipt() {
    for seed in 0..2 {
        let dir = Directory::new(seed);
        let observed = run_child(&dir.0, None);
        assert_eq!(verify(&dir.0, &observed), 16);
    }
}

#[test]
#[allow(clippy::print_stderr)] // Captured per-schedule evidence for a failing CI run.
fn ns1_real_process_kills_preserve_acknowledged_row_batches() {
    clean_controls_recover_every_receipt();
    for seed in 0..8 {
        let dir = Directory::new(seed);
        eprintln!("NS1 process schedule={seed} starting");
        let observed = run_child(&dir.0, Some(seed));
        let recovered = verify(&dir.0, &observed);
        eprintln!(
            "NS1 process schedule={seed}: {} submitted, {} observed acknowledgements, {recovered} recovered",
            observed.submitted.len(),
            observed.acked.len()
        );
        assert!(
            recovered >= 4,
            "schedule {seed} exercised too little history"
        );
    }
}
