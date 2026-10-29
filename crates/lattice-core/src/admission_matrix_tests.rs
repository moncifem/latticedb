//! Hosted-only fault matrix using actual log operations and independent ChangeSet bytes.
use super::*;

#[derive(Clone, Copy, Debug)]
struct Event {
    kind: &'static str,
    bytes: usize,
}
#[derive(Clone, Copy, Debug)]
enum Fault {
    Cut(usize),
    Partial(usize, usize),
    NoSpace(usize),
    Sync(usize),
}
#[derive(Default)]
struct Probe {
    armed: bool,
    events: Vec<Event>,
    fault: Option<Fault>,
    hit: bool,
}
#[derive(Clone)]
struct FaultDisk {
    disk: Disk,
    probe: Rc<RefCell<Probe>>,
}
impl FaultDisk {
    fn before(&self, kind: &'static str, bytes: usize) -> Option<Fault> {
        let mut p = self.probe.borrow_mut();
        if !p.armed {
            return None;
        }
        let site = p.events.len();
        p.events.push(Event { kind, bytes });
        let matches = match p.fault {
            Some(Fault::Cut(n) | Fault::NoSpace(n) | Fault::Sync(n) | Fault::Partial(n, _)) => {
                n == site
            }
            None => false,
        };
        if matches {
            p.hit = true;
            p.fault
        } else {
            None
        }
    }
    fn failure(&self, kind: &'static str) -> Result<(), IoError> {
        match self.before(kind, 0) {
            Some(Fault::Cut(_)) => {
                self.disk.0.borrow_mut().arm_power_cut(0);
                Ok(())
            }
            Some(Fault::Sync(_)) => Err(IoError::Other("scripted fsync failure".into())),
            Some(other) => panic!("wrong fault {other:?} at {kind}"),
            None => Ok(()),
        }
    }
}
impl Io for FaultDisk {
    fn exists(&self, p: &str) -> bool {
        self.disk.exists(p)
    }
    fn len(&self, p: &str) -> Result<u64, IoError> {
        self.disk.len(p)
    }
    fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
        self.disk.read(p)
    }
    fn read_prefix(&self, p: &str, n: usize) -> Result<Vec<u8>, IoError> {
        self.disk.read_prefix(p, n)
    }
    fn append(&mut self, p: &str, b: &[u8]) -> Result<(), IoError> {
        match self.before("append", b.len()) {
            Some(Fault::Cut(_)) => self.disk.0.borrow_mut().arm_power_cut(0),
            Some(Fault::Partial(_, n)) => {
                self.disk.append(p, &b[..n])?;
                return Err(IoError::Other("scripted partial append".into()));
            }
            Some(Fault::NoSpace(_)) => return Err(IoError::NoSpace),
            Some(other) => panic!("wrong append fault {other:?}"),
            None => {}
        }
        self.disk.append(p, b)
    }
    fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
        self.failure("truncate")?;
        self.disk.truncate(p, n)
    }
    fn sync(&mut self, p: &str) -> Result<(), IoError> {
        self.failure("sync")?;
        self.disk.sync(p)
    }
    fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
        self.failure("sync_dir")?;
        self.disk.sync_dir(p)
    }
    fn list(&self, p: &str) -> Result<Vec<String>, IoError> {
        self.disk.list(p)
    }
    fn remove(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.remove(p)
    }
    fn create_dir_all(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.disk.unlock(p)
    }
}

fn export_case(disk: &Disk, case: usize, stage: &str, description: &str, prefix_row: bool) {
    let mut out = lattice_seams::StdIo::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let base = format!("target/wal-admission-cases/{case:06}-{stage}");
    out.create_dir_all(&format!("{base}/wal")).unwrap();
    let files = disk.manifest();
    let guarded = files
        .iter()
        .any(|(_, b)| b.len() >= 16 && b.get(..8) == Some(admission::GUARDED_MAGIC.as_slice()));
    for (name, bytes) in files {
        let p = format!("{base}/wal/{name}");
        out.append(&p, &bytes).unwrap();
        out.sync(&p).unwrap();
    }
    out.append(&format!("{base}/wal/LOCK"), b"").unwrap();
    let mode = if guarded { "guarded" } else { "legacy-any" };
    let metadata = format!(
        "mode={mode}\nallow_mutation={}\nprefix_row={prefix_row}\n{description}\n",
        stage == "raw" && !guarded
    );
    out.append(&format!("{base}/expect.txt"), metadata.as_bytes())
        .unwrap();
}

fn payload(id: u128, compound: bool) -> Vec<u8> {
    let fixture = if compound {
        COMPOUND
    } else {
        include_bytes!("../../../formats/corpus/wal-admission-v1-rows.golden").as_slice()
    };
    let offset = if compound { 32 + 17 } else { 16 + 17 };
    let mut bytes = fixture[offset..].to_vec();
    bytes[1..17].copy_from_slice(&id.to_be_bytes());
    bytes
}

fn run(
    case: usize,
    scenario: usize,
    seed: u64,
    profile: FaultProfile,
    fault: Option<Fault>,
) -> Vec<Event> {
    let disk = Disk(
        Rc::new(RefCell::new(SimIo::new(seed, profile))),
        Rc::new(Cell::new(0)),
    );
    let probe = Rc::new(RefCell::new(Probe::default()));
    let io = FaultDisk {
        disk: disk.clone(),
        probe: probe.clone(),
    };
    let limit = if scenario == 2 { 1 } else { 1 << 20 };
    let (mut log, _) =
        Log::open_streaming_with_capabilities(io, "wal", limit, 1, |_, _| {}).unwrap();
    if scenario == 1 {
        log.append(1, &payload(1, false)).unwrap();
        log.sync().unwrap();
    }
    if scenario == 2 {
        log.append_with_capabilities(1, &payload(2, true), 1)
            .unwrap();
        log.sync().unwrap();
    }
    {
        let mut p = probe.borrow_mut();
        p.armed = true;
        p.fault = fault;
    }
    let id = if scenario == 2 { 3 } else { 2 };
    let body = payload(id, scenario != 2);
    let ack = log
        .append_with_capabilities(1, &body, if scenario == 2 { 0 } else { 1 })
        .and_then(|_| log.sync())
        .is_ok();
    let events = probe.borrow().events.clone();
    if fault.is_some() {
        assert!(
            probe.borrow().hit,
            "control operation not reached: {fault:?}"
        );
    }
    probe.borrow_mut().armed = false;
    drop(log);
    disk.0.borrow_mut().crash();
    let description = format!(
        "scenario={scenario}\nseed={seed}\nprofile={profile:?}\nfault={fault:?}\nack={ack}"
    );
    export_case(&disk, case, "raw", &description, scenario == 1);
    let mut records = Vec::new();
    let (log, _) =
        Log::open_streaming_with_capabilities(disk.clone(), "wal", 1 << 20, 1, |r, c| {
            records.push((r, c))
        })
        .unwrap();
    let ids: Vec<_> = records
        .iter()
        .map(|(r, _)| ChangeSet::decode(&r.payload).unwrap().id)
        .collect();
    assert_eq!(
        ids.iter()
            .filter(|id| **id == ChangeSetId::from_bytes(1u128.to_be_bytes()))
            .count(),
        usize::from(scenario == 1)
    );
    if scenario == 2 {
        assert!(ids.contains(&ChangeSetId::from_bytes(2u128.to_be_bytes())));
    }
    let adopted = records
        .iter()
        .filter(|(r, _)| {
            ChangeSet::decode(&r.payload).unwrap().id == ChangeSetId::from_bytes(id.to_be_bytes())
        })
        .collect::<Vec<_>>();
    assert!(adopted.len() <= 1);
    assert!(!ack || adopted.len() == 1);
    for (r, c) in &records {
        let cs = ChangeSet::decode(&r.payload).unwrap();
        if cs.id != ChangeSetId::from_bytes(1u128.to_be_bytes()) {
            assert_eq!(c.required_capabilities, MANAGED_COMPOUND);
        }
        if cs.id == ChangeSetId::from_bytes(id.to_be_bytes()) {
            assert_eq!(
                r.payload, body,
                "whole independent row/catalog/index envelope"
            );
        }
    }
    assert_eq!(log.read_from_with_context(1, 10).unwrap(), records);
    drop(log);
    export_case(&disk, case, "recovered", &description, scenario == 1);
    let mut second = Vec::new();
    let (_log, _) =
        Log::open_streaming_with_capabilities(disk, "wal", 1 << 20, 1, |r, c| second.push((r, c)))
            .unwrap();
    assert_eq!(second, records);
    events
}

#[test]
#[ignore = "expanded upgrade/rollover failure matrix and frozen-reader snapshots run on hosted CI"]
fn guarded_upgrade_at_every_observed_mutating_cut_has_no_unguarded_adoption() {
    let mut case = 0;
    for profile in [
        FaultProfile::clean(),
        FaultProfile {
            garbage_tail: false,
            ..FaultProfile::crash_heavy()
        },
    ] {
        for seed in 0..4 {
            for scenario in 0..3 {
                let events = run(case, scenario, seed, profile, None);
                case += 1;
                assert!(events.len() >= 3);
                let mut faults = Vec::new();
                for (site, event) in events.iter().enumerate() {
                    faults.push(Fault::Cut(site));
                    if event.kind == "append" {
                        faults.push(Fault::NoSpace(site));
                        for keep in 0..=event.bytes {
                            faults.push(Fault::Partial(site, keep));
                        }
                    }
                    if matches!(event.kind, "sync" | "sync_dir") {
                        faults.push(Fault::Sync(site));
                    }
                }
                for fault in faults {
                    run(case, scenario, seed, profile, Some(fault));
                    case += 1;
                }
            }
        }
    }
    eprintln!(
        "guarded WAL expanded matrix verified {case} histories; raw/recovered snapshots exported for frozen old-reader checks"
    );
}
