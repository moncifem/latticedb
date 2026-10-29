#![no_main]
use lattice_core::changeset::ChangeSet;
use lattice_core::{Config, Kernel};
use lattice_seams::{Clock, Io, Rng};
use lattice_sim::{FaultProfile, SeededRng, SimIo};
use libfuzzer_sys::fuzz_target;

struct Fixed;
impl Clock for Fixed {
    fn now_ms(&self) -> u64 {
        1
    }
}

/// Wraps the simulated disk so the kernel can own it while the target keeps no handle.
struct Disk(SimIo);

impl Io for Disk {
    fn exists(&self, p: &str) -> bool { self.0.exists(p) }
    fn len(&self, p: &str) -> Result<u64, lattice_seams::IoError> { self.0.len(p) }
    fn read(&self, p: &str) -> Result<Vec<u8>, lattice_seams::IoError> { self.0.read(p) }

    fn read_prefix(&self, p: &str, max: usize) -> Result<Vec<u8>, lattice_seams::IoError> { self.0.read_prefix(p, max) }

    fn append(&mut self, p: &str, d: &[u8]) -> Result<(), lattice_seams::IoError> { self.0.append(p, d) }
    fn truncate(&mut self, p: &str, l: u64) -> Result<(), lattice_seams::IoError> { self.0.truncate(p, l) }
    fn sync(&mut self, p: &str) -> Result<(), lattice_seams::IoError> { self.0.sync(p) }
    fn sync_dir(&mut self, p: &str) -> Result<(), lattice_seams::IoError> { self.0.sync_dir(p) }
    fn list(&self, p: &str) -> Result<Vec<String>, lattice_seams::IoError> { self.0.list(p) }
    fn remove(&mut self, p: &str) -> Result<(), lattice_seams::IoError> { self.0.remove(p) }
    fn create_dir_all(&mut self, p: &str) -> Result<(), lattice_seams::IoError> { self.0.create_dir_all(p) }
    fn lock(&mut self, p: &str) -> Result<(), lattice_seams::IoError> { self.0.lock(p) }
    fn unlock(&mut self, p: &str) { self.0.unlock(p) }
}

fuzz_target!(|data: &[u8]| {
    let mut io = SimIo::new(1, FaultProfile::clean());
    let _ = io.create_dir_all("wal");
    let _ = io.append("wal/00000000000000000001.seg", data);
    let config = Config { dir: "wal".into(), segment_limit: 1 << 20, node: 1 };
    if let Ok((mut k, _)) = Kernel::open(Disk(io), Fixed, SeededRng::new(1), &config) {
        let id = k.new_id();
        let _ = k.commit(ChangeSet::new(id, vec![1]));
    }
    let _ = SeededRng::new(0).next_u64();
});
