//! The benchmarks themselves.

use lattice_abi::Hlc;
use lattice_core::changeset::{ChangeSet, ChangeSetId};
use lattice_core::crdt::{Crdt, OrSet};
use lattice_core::record::{decode_record, encode_record};
use lattice_core::{Config, HlcClock, Kernel};
use lattice_lsl::{check::check, parser::parse};
use lattice_seams::{Clock, Io, IoError, Rng};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::rc::Rc;
use std::time::Instant;

struct Clk(Rc<SimClock>);
impl Clock for Clk {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

/// Times `op` and returns the median nanoseconds per call over `rounds` rounds of `iters` calls.
fn time(rounds: usize, iters: u64, mut op: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..iters {
                op();
            }
            start.elapsed().as_nanos() as f64 / iters as f64
        })
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    samples.get(samples.len() / 2).copied().unwrap_or(f64::NAN)
}

const SCHEMA: &str = "enum P { low, medium, high }\nfacet E { n: int in 0..=100 optional  p: P }\ntype Epic { title: text max 120 }\ntype Story = E { title: text max 200  epic: ref<Epic> optional  acceptance: list<text> max 20 }\n";

/// A simulated disk that outlives the kernel using it, so a benchmark can reopen a populated disk.
struct SharedSim(Rc<RefCell<SimIo>>);

impl Io for SharedSim {
    fn exists(&self, path: &str) -> bool {
        self.0.borrow().exists(path)
    }
    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.0.borrow().len(path)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        self.0.borrow().read(path)
    }
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.0.borrow_mut().append(path, data)
    }
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.0.borrow_mut().truncate(path, len)
    }
    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync(path)
    }
    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync_dir(dir)
    }
    fn list(&self, dir: &str) -> Result<Vec<String>, IoError> {
        self.0.borrow().list(dir)
    }
    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().remove(path)
    }
    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError> {
        self.0.borrow_mut().create_dir_all(dir)
    }
    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().lock(path)
    }
    fn unlock(&mut self, path: &str) {
        self.0.borrow_mut().unlock(path);
    }
}

fn open_on<I: Io>(io: I) -> Kernel<I, Clk, SeededRng> {
    Kernel::open(
        io,
        Clk(Rc::new(SimClock::new(1))),
        SeededRng::new(1),
        &Config::default(),
    )
    .map(|(k, _)| k)
    .unwrap_or_else(|e| unreachable!("fresh simulated disk must open: {e}"))
}

fn open_kernel() -> Kernel<SimIo, Clk, SeededRng> {
    let io = SimIo::new(1, FaultProfile::clean());
    Kernel::open(
        io,
        Clk(Rc::new(SimClock::new(1))),
        SeededRng::new(1),
        &Config::default(),
    )
    .map(|(k, _)| k)
    .unwrap_or_else(|e| unreachable!("fresh simulated disk must open: {e}"))
}

/// Runs every benchmark. `scale` multiplies iteration counts (use a small value in tests).
pub fn run_all(scale: u64) -> BTreeMap<String, f64> {
    let s = scale.max(1);
    let mut out = BTreeMap::new();

    let payload = vec![7u8; 100];
    out.insert(
        "record_encode_decode_100b".to_string(),
        time(7, 20_000 * s, || {
            let bytes = encode_record(black_box(42), 1, black_box(&payload));
            black_box(decode_record(&bytes, 0));
        }),
    );

    let mut clock = HlcClock::new(1);
    let mut now = 0u64;
    out.insert(
        "hlc_tick".to_string(),
        time(7, 200_000 * s, || {
            now += 1;
            let _ = black_box(clock.tick(now));
        }),
    );

    out.insert(
        "lsl_parse_and_check".to_string(),
        time(7, 2_000 * s, || {
            if let Ok(schema) = parse(black_box(SCHEMA)) {
                let _ = black_box(check(&schema));
            }
        }),
    );

    let mut a = OrSet::default();
    let mut b = OrSet::default();
    for i in 0..500u32 {
        a.add(
            i,
            Hlc {
                wall_ms: u64::from(i),
                counter: 0,
                node: 1,
            },
        );
        b.add(
            i + 250,
            Hlc {
                wall_ms: u64::from(i),
                counter: 0,
                node: 2,
            },
        );
    }
    out.insert(
        "orset_merge_500".to_string(),
        time(7, 200 * s, || {
            let mut m = a.clone();
            m.merge(black_box(&b));
            black_box(m);
        }),
    );

    // The commit path on simulated storage: all the kernel's work, none of the disk's latency.
    let mut kernel = open_kernel();
    let mut rng = SeededRng::new(9);
    out.insert(
        "commit_single_cpu".to_string(),
        time(5, 2_000 * s, || {
            let id = ChangeSetId::from_raw(u128::from(rng.next_u64()));
            let _ = black_box(kernel.commit(ChangeSet::new(id, vec![1; 100])));
        }),
    );
    let mut kernel = open_kernel();
    out.insert(
        "commit_batch_of_16_cpu_per_item".to_string(),
        time(5, 200 * s, || {
            let batch: Vec<ChangeSet> = (0..16)
                .map(|_| {
                    ChangeSet::new(
                        ChangeSetId::from_raw(u128::from(rng.next_u64())),
                        vec![1; 100],
                    )
                })
                .collect();
            let _ = black_box(kernel.commit_batch(batch));
        }) / 16.0,
    );

    // Recovery cost grows with the log until checkpoints exist; track it per record. Each round writes
    // a populated disk (not timed) and times only the reopen that has to read it back.
    let records = 2_000u64;
    let mut samples: Vec<f64> = (0..5 * 3 * s)
        .map(|_| {
            let disk = Rc::new(RefCell::new(SimIo::new(1, FaultProfile::clean())));
            let mut seed = open_on(SharedSim(disk.clone()));
            for _ in 0..records {
                let id = seed.new_id();
                let _ = seed.commit(ChangeSet::new(id, vec![1; 100]));
            }
            drop(seed);
            let io = SharedSim(disk.clone());
            let start = Instant::now();
            let reopened = Kernel::open(
                io,
                Clk(Rc::new(SimClock::new(1))),
                SeededRng::new(1),
                &Config::default(),
            )
            .map(|(k, rec)| (k, rec.records.len()));
            assert!(
                reopened.as_ref().is_ok_and(|(_, n)| *n as u64 == records),
                "recovery must read every record back"
            );
            let ns = start.elapsed().as_nanos() as f64;
            black_box(reopened.is_ok());
            ns / records as f64
        })
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out.insert(
        "recovery_open_per_record".to_string(),
        samples.get(samples.len() / 2).copied().unwrap_or(f64::NAN),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_benchmark_runs_and_reports_a_positive_finite_time() {
        let r = run_all(1);
        assert!(r.len() >= 7);
        for (name, ns) in r {
            assert!(ns.is_finite() && ns > 0.0, "{name}: {ns}");
        }
    }
}
