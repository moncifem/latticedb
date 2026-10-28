//! Simulated storage with a durable and a volatile part, and fault injection.
//!
//! Model: a file's content is `durable` (truncated to `trunc` if a truncate is unsynced) followed by
//! `pending` (appended, unsynced). `sync` folds both into `durable`. A crash drops the volatile
//! part, possibly keeping a torn prefix of it. Directory entries (file and directory creations,
//! removals) become durable only through `sync_dir` on the parent.

use crate::{SeededRng, TraceHash};
use lattice_seams::{Io, IoError, Rng};
use std::collections::{BTreeMap, BTreeSet};

/// Which faults a [`SimIo`] injects. Rates are parts per million per operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct FaultProfile {
    /// A crash keeps a random prefix of the unsynced bytes instead of dropping all of them.
    pub torn_writes: bool,
    /// A torn tail may end with a few garbage bytes (a half-written sector).
    pub garbage_tail: bool,
    /// Entries (files, directories) whose creation was not made durable by `sync_dir` vanish on
    /// crash, and removed files whose removal was not made durable reappear.
    pub lose_unsynced_dirs: bool,
    /// Chance that `sync` returns an error (the honest kind of fsync failure).
    pub sync_error_ppm: u32,
    /// Chance that `append` fails; half of the time a random prefix was written first.
    pub append_error_ppm: u32,
    /// `append` fails with "no space" once this many bytes exist.
    pub disk_full_after: Option<usize>,
    /// Chance, per crash, that one random bit of one durable file flips.
    pub bit_rot_ppm: u32,
}

impl FaultProfile {
    /// No faults except that crashes drop all unsynced data.
    pub fn clean() -> Self {
        Self::default()
    }

    /// Torn writes and lost directory entries on crash.
    pub fn crash_heavy() -> Self {
        Self {
            torn_writes: true,
            garbage_tail: true,
            lose_unsynced_dirs: true,
            ..Self::default()
        }
    }

    /// A flaky disk: errors on append and sync, and a small device.
    pub fn disk_faulty() -> Self {
        Self {
            sync_error_ppm: 20_000,
            append_error_ppm: 20_000,
            disk_full_after: Some(24 * 1024),
            ..Self::default()
        }
    }

    /// Everything at once, including bit rot.
    pub fn everything() -> Self {
        Self {
            torn_writes: true,
            garbage_tail: true,
            lose_unsynced_dirs: true,
            sync_error_ppm: 15_000,
            append_error_ppm: 15_000,
            disk_full_after: Some(16 * 1024),
            bit_rot_ppm: 5_000,
        }
    }

    /// The same profile without the transient faults (errors, full disk, rot), used for the
    /// terminal verification of a scenario so every run ends with a real check.
    pub fn without_transient_faults(self) -> Self {
        Self {
            sync_error_ppm: 0,
            append_error_ppm: 0,
            disk_full_after: None,
            bit_rot_ppm: 0,
            ..self
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SimFile {
    durable: Vec<u8>,
    /// An unsynced truncate: the durable content is cut to this length once synced.
    trunc: Option<usize>,
    pending: Vec<u8>,
    entry_durable: bool,
}

impl SimFile {
    fn effective_durable_len(&self) -> usize {
        self.trunc
            .map_or(self.durable.len(), |t| t.min(self.durable.len()))
    }

    fn content(&self) -> Vec<u8> {
        let mut v = self
            .durable
            .get(..self.effective_durable_len())
            .unwrap_or(&[])
            .to_vec();
        v.extend_from_slice(&self.pending);
        v
    }

    fn len(&self) -> usize {
        self.effective_durable_len() + self.pending.len()
    }
}

/// Simulated filesystem. Create one per scenario and call [`SimIo::crash`] to simulate a power
/// cut; the same value then serves as the "disk" that recovery reads.
#[derive(Debug)]
pub struct SimIo {
    files: BTreeMap<String, SimFile>,
    ghosts: BTreeMap<String, Vec<u8>>,
    /// Directory name to whether its creation is durable.
    dirs: BTreeMap<String, bool>,
    locks: BTreeSet<String>,
    rng: SeededRng,
    profile: FaultProfile,
    trace: TraceHash,
    rot_events: u64,
    no_space_events: u64,
    power_cut_after: Option<u64>,
    powered_off: bool,
    power_cuts: u64,
    ops: u64,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

impl SimIo {
    /// Creates an empty disk.
    pub fn new(seed: u64, profile: FaultProfile) -> Self {
        Self {
            files: BTreeMap::new(),
            ghosts: BTreeMap::new(),
            dirs: BTreeMap::new(),
            locks: BTreeSet::new(),
            rng: SeededRng::new(seed),
            profile,
            trace: TraceHash::default(),
            rot_events: 0,
            no_space_events: 0,
            power_cut_after: None,
            powered_off: false,
            power_cuts: 0,
            ops: 0,
        }
    }

    /// Fingerprint of every operation, its data and its outcome, for the determinism self-check.
    pub fn trace_hash(&self) -> u64 {
        self.trace.value()
    }

    /// How many bits bit rot has flipped so far. Durability can only be promised while this is 0.
    pub fn rot_events(&self) -> u64 {
        self.rot_events
    }

    /// How many appends were refused because the simulated disk was full.
    pub fn no_space_events(&self) -> u64 {
        self.no_space_events
    }

    /// How many power cuts hit in the middle of an operation sequence.
    pub fn power_cuts(&self) -> u64 {
        self.power_cuts
    }

    /// Changes the active fault profile.
    pub fn set_profile(&mut self, profile: FaultProfile) {
        self.profile = profile;
    }

    /// The active fault profile.
    pub fn profile(&self) -> FaultProfile {
        self.profile
    }

    /// Cuts the power after `ops` more mutating operations. The operation that hits zero does not
    /// take effect (an append may leave a torn prefix), and every operation after it fails until
    /// [`SimIo::crash`] restarts the machine. This puts crashes inside the durability protocols.
    pub fn arm_power_cut(&mut self, ops: u64) {
        self.power_cut_after = Some(ops);
    }

    /// Mutating operations attempted so far (appends, syncs, truncates, removals). A sweep uses it
    /// to enumerate every point at which power could be cut.
    pub fn mutating_ops(&self) -> u64 {
        self.ops
    }

    /// Whether a power cut has hit and the machine is still dead.
    pub fn is_powered_off(&self) -> bool {
        self.powered_off
    }

    /// Restarts the process without losing power: nothing volatile is lost (the page cache
    /// survives), only locks and the armed power cut are cleared. Not valid after a power cut.
    pub fn restart_process(&mut self) {
        self.trace.write(b"restart");
        self.locks.clear();
        self.power_cut_after = None;
    }

    /// Total bytes stored (durable and pending).
    pub fn used_bytes(&self) -> usize {
        self.files.values().map(SimFile::len).sum()
    }

    fn gate(&mut self, what: &str) -> Result<(), IoError> {
        self.ops += 1;
        if self.powered_off {
            return Err(IoError::Other(format!("power is off ({what})")));
        }
        match self.power_cut_after {
            Some(0) => {
                self.powered_off = true;
                self.power_cuts += 1;
                self.power_cut_after = None;
                self.trace.write(b"!power");
                Err(IoError::Other(format!("power cut during {what}")))
            }
            Some(n) => {
                self.power_cut_after = Some(n - 1);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Simulates a power cut and restart: unsynced bytes are lost (or partly kept when torn writes
    /// are on), unsynced truncates are undone, directory entries that were never synced may
    /// vanish, locks are released, and bit rot may strike.
    pub fn crash(&mut self) {
        self.trace.write(b"crash");
        self.powered_off = false;
        self.power_cut_after = None;
        self.locks.clear();
        let profile = self.profile;
        let mut gone = Vec::new();
        for (name, f) in &mut self.files {
            let dir_lost = profile.lose_unsynced_dirs
                && (!f.entry_durable
                    || self
                        .dirs
                        .iter()
                        .any(|(d, durable)| !durable && name.starts_with(&format!("{d}/"))));
            if dir_lost {
                gone.push(name.clone());
                continue;
            }
            f.trunc = None;
            if profile.torn_writes && !f.pending.is_empty() {
                let keep = self.rng.below(f.pending.len() as u64 + 1) as usize;
                f.durable
                    .extend_from_slice(f.pending.get(..keep).unwrap_or(&[]));
                if profile.garbage_tail && self.rng.chance_ppm(500_000) {
                    for _ in 0..self.rng.below(9) {
                        let b = (self.rng.next_u64() & 0xFF) as u8;
                        f.durable.push(b);
                    }
                }
            }
            f.pending.clear();
        }
        for name in gone {
            self.files.remove(&name);
        }
        if profile.lose_unsynced_dirs {
            self.dirs.retain(|_, durable| *durable);
            for (name, content) in std::mem::take(&mut self.ghosts) {
                self.files.insert(
                    name,
                    SimFile {
                        durable: content,
                        trunc: None,
                        pending: Vec::new(),
                        entry_durable: true,
                    },
                );
            }
        } else {
            self.ghosts.clear();
        }
        if profile.bit_rot_ppm > 0 && self.rng.chance_ppm(profile.bit_rot_ppm) {
            let names: Vec<String> = self.files.keys().cloned().collect();
            if let Some(name) = names.get(self.rng.below(names.len() as u64) as usize)
                && let Some(f) = self.files.get_mut(name)
                && !f.durable.is_empty()
            {
                let pos = self.rng.below(f.durable.len() as u64) as usize;
                let bit = self.rng.below(8) as u8;
                if let Some(byte) = f.durable.get_mut(pos) {
                    *byte ^= 1 << bit;
                    self.rot_events += 1;
                }
            }
        }
        self.mix_state();
    }

    fn mix_state(&mut self) {
        for (name, f) in &self.files {
            self.trace.write(name.as_bytes());
            self.trace.write_u64(f.durable.len() as u64);
            self.trace.write(&f.durable);
        }
    }

    /// The durable content of a file, as recovery would see it right after a crash.
    pub fn durable_content(&self, path: &str) -> Option<Vec<u8>> {
        self.files.get(path).map(|f| f.durable.clone())
    }
}

impl Io for SimIo {
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.files
            .get(path)
            .map(|f| f.len() as u64)
            .ok_or_else(|| IoError::NotFound(path.into()))
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        self.files
            .get(path)
            .map(SimFile::content)
            .ok_or_else(|| IoError::NotFound(path.into()))
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.trace.write(b"append");
        self.trace.write(path.as_bytes());
        self.trace.write_u64(data.len() as u64);
        self.trace.write(data);
        if self.powered_off || self.power_cut_after == Some(0) {
            // A cut during an append may leave a torn prefix of the data behind.
            let keep = if self.rng.below(2) == 0 {
                0
            } else {
                self.rng.below(data.len() as u64 + 1) as usize
            };
            if self.power_cut_after == Some(0) {
                self.files
                    .entry(path.to_string())
                    .or_default()
                    .pending
                    .extend_from_slice(data.get(..keep).unwrap_or(&[]));
            }
            return self.gate("append");
        }
        self.gate("append")?;
        if self.rng.chance_ppm(self.profile.append_error_ppm) {
            self.trace.write(b"!err");
            if self.rng.below(2) == 0 {
                let keep = self.rng.below(data.len() as u64 + 1) as usize;
                self.trace.write_u64(keep as u64);
                self.files
                    .entry(path.to_string())
                    .or_default()
                    .pending
                    .extend_from_slice(data.get(..keep).unwrap_or(&[]));
            }
            return Err(IoError::Other("injected append error".into()));
        }
        if let Some(limit) = self.profile.disk_full_after
            && self.used_bytes() + data.len() > limit
        {
            self.trace.write(b"!full");
            self.no_space_events += 1;
            return Err(IoError::NoSpace);
        }
        self.files
            .entry(path.to_string())
            .or_default()
            .pending
            .extend_from_slice(data);
        Ok(())
    }

    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.trace.write(b"truncate");
        self.trace.write(path.as_bytes());
        self.trace.write_u64(len);
        self.gate("truncate")?;
        let f = self
            .files
            .get_mut(path)
            .ok_or_else(|| IoError::NotFound(path.into()))?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let eff = f.effective_durable_len();
        // Like real storage, a truncate is not durable until the next sync.
        if len <= eff {
            f.trunc = Some(len);
            f.pending.clear();
        } else {
            f.pending.truncate(len - eff);
        }
        Ok(())
    }

    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        self.trace.write(b"sync");
        self.trace.write(path.as_bytes());
        self.gate("sync")?;
        if self.rng.chance_ppm(self.profile.sync_error_ppm) {
            self.trace.write(b"!err");
            // After a failed fsync the kernel may mark the dirty pages clean, so the unsynced bytes
            // are gone and a later fsync reports success without them.
            if let Some(f) = self.files.get_mut(path) {
                f.pending.clear();
            }
            return Err(IoError::Other("injected fsync error".into()));
        }
        let f = self
            .files
            .get_mut(path)
            .ok_or_else(|| IoError::NotFound(path.into()))?;
        if let Some(t) = f.trunc.take() {
            f.durable.truncate(t);
        }
        let pending = std::mem::take(&mut f.pending);
        f.durable.extend_from_slice(&pending);
        Ok(())
    }

    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError> {
        self.trace.write(b"sync_dir");
        self.trace.write(dir.as_bytes());
        self.gate("sync_dir")?;
        if self.rng.chance_ppm(self.profile.sync_error_ppm) {
            self.trace.write(b"!err");
            return Err(IoError::Other("injected dir fsync error".into()));
        }
        for (name, f) in &mut self.files {
            if parent(name) == dir {
                f.entry_durable = true;
            }
        }
        for (name, durable) in &mut self.dirs {
            if parent(name) == dir {
                *durable = true;
            }
        }
        self.ghosts.retain(|name, _| parent(name) != dir);
        Ok(())
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, IoError> {
        if !dir.is_empty() && !self.dirs.contains_key(dir) {
            return Err(IoError::NotFound(dir.into()));
        }
        Ok(self
            .files
            .keys()
            .filter(|n| parent(n) == dir)
            .filter_map(|n| n.rsplit('/').next().map(String::from))
            .collect())
    }

    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        self.trace.write(b"remove");
        self.trace.write(path.as_bytes());
        self.gate("remove")?;
        let f = self
            .files
            .remove(path)
            .ok_or_else(|| IoError::NotFound(path.into()))?;
        if f.entry_durable {
            self.ghosts.insert(path.to_string(), f.durable);
        }
        Ok(())
    }

    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError> {
        let mut cur = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(part);
            self.dirs.entry(cur.clone()).or_insert(false);
        }
        Ok(())
    }

    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        if self.locks.contains(path) {
            return Err(IoError::Other(format!(
                "{path} is locked by another writer"
            )));
        }
        self.locks.insert(path.to_string());
        Ok(())
    }

    fn unlock(&mut self, path: &str) {
        self.locks.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_io_satisfies_the_contract() {
        let mut io = SimIo::new(1, FaultProfile::clean());
        lattice_seams::conformance::run(&mut io);
    }

    #[test]
    fn unsynced_bytes_are_lost_on_crash_and_synced_bytes_survive() {
        let mut io = SimIo::new(1, FaultProfile::clean());
        io.create_dir_all("d").unwrap();
        io.append("d/f", b"durable").unwrap();
        io.sync("d/f").unwrap();
        io.sync_dir("d").unwrap();
        io.append("d/f", b"-volatile").unwrap();
        assert_eq!(io.read("d/f").unwrap(), b"durable-volatile");
        io.crash();
        assert_eq!(io.read("d/f").unwrap(), b"durable");
    }

    #[test]
    fn an_unsynced_truncate_is_undone_by_a_crash() {
        let mut io = SimIo::new(1, FaultProfile::clean());
        io.create_dir_all("d").unwrap();
        io.append("d/f", b"0123456789").unwrap();
        io.sync("d/f").unwrap();
        io.truncate("d/f", 4).unwrap();
        assert_eq!(io.read("d/f").unwrap(), b"0123");
        io.crash();
        assert_eq!(
            io.read("d/f").unwrap(),
            b"0123456789",
            "the old tail reappears"
        );
        io.truncate("d/f", 4).unwrap();
        io.sync("d/f").unwrap();
        io.crash();
        assert_eq!(io.read("d/f").unwrap(), b"0123");
    }

    #[test]
    fn unsynced_directory_entries_vanish_when_the_profile_says_so() {
        let mut io = SimIo::new(1, FaultProfile::crash_heavy());
        io.create_dir_all("d").unwrap();
        io.append("d/f", b"x").unwrap();
        io.sync("d/f").unwrap();
        io.crash();
        assert!(!io.exists("d/f"), "file creation was never made durable");
        assert!(
            io.list("d").is_err(),
            "the directory itself was never made durable either"
        );
        // With the parent synced, both survive.
        io.create_dir_all("d").unwrap();
        io.sync_dir("").unwrap();
        io.append("d/f", b"x").unwrap();
        io.sync("d/f").unwrap();
        io.sync_dir("d").unwrap();
        io.crash();
        assert_eq!(io.read("d/f").unwrap(), b"x");
    }

    #[test]
    fn torn_writes_keep_a_prefix_of_pending_bytes() {
        let mut kept_some = false;
        let mut dropped_some = false;
        for seed in 0..64 {
            let mut io = SimIo::new(
                seed,
                FaultProfile {
                    torn_writes: true,
                    ..FaultProfile::clean()
                },
            );
            io.create_dir_all("d").unwrap();
            io.append("d/f", b"0123456789").unwrap();
            io.crash();
            let len = io.read("d/f").unwrap().len();
            assert!(len <= 10);
            kept_some |= len > 0;
            dropped_some |= len < 10;
        }
        assert!(kept_some && dropped_some);
    }

    #[test]
    fn disk_full_and_error_injection_including_partial_appends() {
        let mut io = SimIo::new(
            1,
            FaultProfile {
                disk_full_after: Some(4),
                ..FaultProfile::clean()
            },
        );
        assert!(io.append("f", b"abcd").is_ok());
        assert_eq!(io.append("f", b"e"), Err(IoError::NoSpace));
        let mut bad = SimIo::new(
            1,
            FaultProfile {
                sync_error_ppm: 1_000_000,
                ..FaultProfile::clean()
            },
        );
        bad.append("f", b"a").unwrap();
        assert!(bad.sync("f").is_err());
        let mut saw_partial = false;
        for seed in 0..64 {
            let mut p = SimIo::new(
                seed,
                FaultProfile {
                    append_error_ppm: 1_000_000,
                    ..FaultProfile::clean()
                },
            );
            assert!(p.append("f", b"0123456789").is_err());
            if let Ok(c) = p.read("f") {
                saw_partial |= !c.is_empty();
            }
        }
        assert!(saw_partial, "a failed append may leave a prefix behind");
    }

    #[test]
    fn a_power_cut_lands_inside_an_operation_sequence_and_kills_later_operations() {
        let mut io = SimIo::new(3, FaultProfile::crash_heavy());
        io.create_dir_all("d").unwrap();
        io.arm_power_cut(2);
        assert!(io.append("d/f", b"a").is_ok());
        assert!(io.sync("d/f").is_ok());
        assert!(io.sync_dir("d").is_err(), "the third mutation is cut");
        assert!(
            io.append("d/f", b"b").is_err(),
            "the machine stays dead until restart"
        );
        assert_eq!(io.power_cuts(), 1);
        io.crash();
        assert!(
            io.append("d/f", b"c").is_ok(),
            "power is back after the restart"
        );
    }

    #[test]
    fn locks_exclude_a_second_writer_and_vanish_on_crash() {
        let mut io = SimIo::new(1, FaultProfile::clean());
        assert!(io.lock("d/LOCK").is_ok());
        assert!(io.lock("d/LOCK").is_err());
        io.unlock("d/LOCK");
        assert!(io.lock("d/LOCK").is_ok());
        io.crash();
        assert!(io.lock("d/LOCK").is_ok(), "a dead process holds no lock");
    }

    #[test]
    fn the_same_seed_gives_the_same_trace_and_a_different_seed_does_not() {
        let run = |seed| {
            let mut io = SimIo::new(seed, FaultProfile::everything());
            io.create_dir_all("d").unwrap();
            for i in 0..50u8 {
                let _ = io.append("d/f", &[i; 7]);
                let _ = io.sync("d/f");
                if i % 7 == 0 {
                    io.crash();
                }
            }
            (io.trace_hash(), io.read("d/f").unwrap_or_default())
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5).0, run(6).0);
    }

    #[test]
    fn the_trace_distinguishes_runs_that_differ_only_in_data() {
        let run = |byte| {
            let mut io = SimIo::new(1, FaultProfile::clean());
            io.create_dir_all("d").unwrap();
            io.append("d/f", &[byte; 16]).unwrap();
            io.sync("d/f").unwrap();
            io.crash();
            io.trace_hash()
        };
        assert_ne!(run(1), run(2));
    }
}
