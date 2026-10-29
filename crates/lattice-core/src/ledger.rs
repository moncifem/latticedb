//! A results ledger stored in Lattice's own log. Simulation, benchmark and eval runs append their
//! summaries as change sets; the scorecard reads them back, so the project's health history is kept
//! by the thing it measures and can be verified with the same tools (`lattice doctor`).

use crate::changeset::ChangeSet;
use crate::kernel::{Config, Kernel, Receipt};
use crate::record::RecordType;
use crate::txn::TxnError;
use lattice_seams::{Clock, Io, Rng};

/// One recorded result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    /// Position in the ledger.
    pub lsn: u64,
    /// Wall-clock milliseconds when it was recorded.
    pub recorded_ms: u64,
    /// What produced it (`sim`, `bench`, ...).
    pub kind: String,
    /// The summary, as JSON text.
    pub json: String,
}

fn config() -> Config {
    Config {
        dir: "ledger".into(),
        segment_limit: 1 << 20,
        node: 0,
    }
}

/// Appends a summary to the ledger stored under `io`.
pub fn append<I: Io, C: Clock, R: Rng>(
    io: I,
    clock: C,
    rng: R,
    kind: &str,
    json: &str,
) -> Result<Receipt, TxnError> {
    let (mut kernel, _) = Kernel::open(io, clock, rng, &config())?;
    let id = kernel.new_id();
    kernel.commit(ChangeSet::new(id, format!("{kind}\n{json}").into_bytes()))
}

/// Reads every entry of the ledger stored under `io`.
pub fn read<I: Io, C: Clock, R: Rng>(
    io: I,
    clock: C,
    rng: R,
) -> Result<Vec<LedgerEntry>, TxnError> {
    let mut out = Vec::new();
    let _ = Kernel::open_streaming(io, clock, rng, &config(), |r| {
        if RecordType::from_u8(r.rtype) != Some(RecordType::ChangeSet) {
            return;
        }
        if let Ok(cs) = ChangeSet::decode(&r.payload)
            && let Ok(text) = String::from_utf8(cs.payload)
            && let Some((kind, json)) = text.split_once('\n')
        {
            out.push(LedgerEntry {
                lsn: r.lsn,
                recorded_ms: cs.hlc.wall_ms,
                kind: kind.to_string(),
                json: json.to_string(),
            });
        }
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use std::rc::Rc;

    struct Clk(Rc<SimClock>);
    impl Clock for Clk {
        fn now_ms(&self) -> u64 {
            self.0.now_ms()
        }
    }

    /// A disk shared by successive opens, as a real directory would be.
    #[derive(Clone)]
    struct Disk(Rc<std::cell::RefCell<SimIo>>);
    impl Io for Disk {
        fn exists(&self, p: &str) -> bool {
            self.0.borrow().exists(p)
        }
        fn len(&self, p: &str) -> Result<u64, lattice_seams::IoError> {
            self.0.borrow().len(p)
        }
        fn read(&self, p: &str) -> Result<Vec<u8>, lattice_seams::IoError> {
            self.0.borrow().read(p)
        }
        fn append(&mut self, p: &str, d: &[u8]) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().append(p, d)
        }
        fn truncate(&mut self, p: &str, l: u64) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().truncate(p, l)
        }
        fn sync(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().sync(p)
        }
        fn sync_dir(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().sync_dir(p)
        }
        fn list(&self, p: &str) -> Result<Vec<String>, lattice_seams::IoError> {
            self.0.borrow().list(p)
        }
        fn remove(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().remove(p)
        }
        fn create_dir_all(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().create_dir_all(p)
        }
        fn lock(&mut self, p: &str) -> Result<(), lattice_seams::IoError> {
            self.0.borrow_mut().lock(p)
        }
        fn unlock(&mut self, p: &str) {
            self.0.borrow_mut().unlock(p)
        }
    }

    #[test]
    fn entries_survive_reopening_in_order() {
        let disk = Disk(Rc::new(std::cell::RefCell::new(SimIo::new(
            1,
            FaultProfile::clean(),
        ))));
        let clock = Rc::new(SimClock::new(5_000));
        for (i, kind) in ["sim", "bench", "sim"].iter().enumerate() {
            let r = append(
                disk.clone(),
                Clk(clock.clone()),
                SeededRng::new(i as u64 + 1),
                kind,
                &format!("{{\"n\":{i}}}"),
            );
            assert!(matches!(r, Ok(Receipt::Recorded { .. })), "{r:?}");
            clock.advance(10);
        }
        let entries = read(disk, Clk(clock), SeededRng::new(99)).unwrap();
        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["sim", "bench", "sim"]);
        assert_eq!(entries[1].json, "{\"n\":1}");
        assert!(
            entries
                .windows(2)
                .all(|w| w[0].recorded_ms < w[1].recorded_ms && w[0].lsn < w[1].lsn)
        );
    }
}
