//! Real-file row-engine microbenchmarks; preparation and semantic checks are untimed.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use lattice_bench::{compare, parse_results, render_results};
use lattice_core::Config;
use lattice_engines::index::IndexDef;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::{OsRng, Rng, StdIo, SystemClock};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

type Store = RowStore<StdIo, SystemClock, OsRng>;
type Results = BTreeMap<String, f64>;
const TABLE: u32 = 1;
const INDEX: u32 = 1;
const BUCKETS: u64 = 16;
const COMMITS: u64 = 16;

#[derive(Clone, Debug)]
struct Options {
    profile: String,
    rows: u64,
    reads: u64,
    samples: usize,
    tolerance: f64,
    write: bool,
}
impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut out = Self {
            profile: "local".into(),
            rows: 1024,
            reads: 10_000,
            samples: 5,
            tolerance: 0.25,
            write: false,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--write-baseline" {
                out.write = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?;
            match arg.as_str() {
                "--profile" => out.profile = value,
                "--rows" => out.rows = value.parse().map_err(|_| "invalid rows")?,
                "--reads" => out.reads = value.parse().map_err(|_| "invalid reads")?,
                "--samples" => out.samples = value.parse().map_err(|_| "invalid samples")?,
                "--tolerance" => out.tolerance = value.parse().map_err(|_| "invalid tolerance")?,
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        if out.profile.is_empty()
            || out.profile.len() > 64
            || !out
                .profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(
                "profile must contain 1..=64 ASCII letters, digits, dashes or underscores".into(),
            );
        }
        if !(1..=100_000).contains(&out.rows)
            || !(1..=10_000_000).contains(&out.reads)
            || !(1..=31).contains(&out.samples)
        {
            return Err("require rows1..=100000, reads1..=10000000, samples1..=31".into());
        }
        if !out.tolerance.is_finite() || out.tolerance < 0.0 {
            return Err("tolerance must be finite and nonnegative".into());
        }
        Ok(out)
    }
    fn metadata(&self) -> Value {
        json!({"schema":1,"profile":self.profile,"rows":self.rows,"reads":self.reads,
               "samples":self.samples,"commits_per_sample":COMMITS,"value_bytes":64,
               "index_buckets":BUCKETS,"segment_limit_bytes":config().segment_limit,
               "read_versions":4,"range_keys":16,"recovery_versions":[1,4,16],"vacuum_versions":16,
               "index_bucket_rotates_each_version":true,
               "architecture":std::env::consts::ARCH,"os":std::env::consts::OS,
               "debug_assertions":cfg!(debug_assertions),
               "backend":"StdIo","cache":"same process; shared warm/uncontrolled filesystem cache"})
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "lattice-row-bench-{}-{:016x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn config() -> Config {
    Config {
        segment_limit: 64 * 1024,
        ..Config::default()
    }
}
fn extract(value: &[u8]) -> Option<Vec<u8>> {
    value.get(..8).map(<[u8]>::to_vec)
}
fn open(path: &Path) -> Result<Store, String> {
    RowStore::open_with_indexes(
        StdIo::new(path),
        SystemClock,
        OsRng,
        &config(),
        vec![IndexDef {
            id: INDEX,
            table: TABLE,
            extract,
        }],
    )
    .map_err(|e| e.to_string())
}
fn key(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}
fn value(n: u64, generation: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(&((n + generation) % BUCKETS).to_le_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&n.to_le_bytes());
    bytes.resize(64, 0xA5);
    bytes
}
fn prepare(path: &Path, rows: u64, versions: u64) -> Result<Store, String> {
    let mut store = open(path)?;
    for generation in 0..versions {
        let mut tx = store.begin();
        for n in 0..rows {
            tx.put((TABLE, key(n)), value(n, generation));
        }
        store.commit(tx).map_err(|e| e.to_string())?;
    }
    if store.applied_lsn() != versions {
        return Err("prepared change-set count differs".into());
    }
    verify(&store, rows, versions - 1)?;
    Ok(store)
}
fn verify(store: &Store, rows: u64, generation: u64) -> Result<(), String> {
    let snap = store.snapshot();
    let scanned = store
        .scan_at(snap, TABLE, .., &lattice_engines::limits::AllowAll)
        .map_err(|e| e.to_string())?;
    if scanned.len() as u64 != rows {
        return Err("live row count differs".into());
    }
    for n in 0..rows {
        if store
            .get_at(snap, TABLE, &key(n), &lattice_engines::limits::AllowAll)
            .map_err(|e| e.to_string())?
            != Some(&value(n, generation))
        {
            return Err(format!("row {n} differs"));
        }
    }
    // Audit the same bounded range used by the timer; an unbounded count alone would miss an
    // incorrect exclusive upper bound or a no-op bounded scan becoming deceptively faster.
    let end = rows.min(16);
    let bounded = store
        .scan_at(
            snap,
            TABLE,
            key(0)..key(end),
            &lattice_engines::limits::AllowAll,
        )
        .map_err(|e| e.to_string())?;
    if bounded.len() as u64 != end
        || bounded.iter().enumerate().any(|(n, (pk, row))| {
            *pk != (n as u64).to_be_bytes().as_slice() || *row != &value(n as u64, generation)
        })
    {
        return Err("bounded range rows differ".into());
    }
    for bucket in 0..BUCKETS {
        let found = store
            .lookup_at(
                snap,
                INDEX,
                &bucket.to_le_bytes(),
                &lattice_engines::limits::AllowAll,
            )
            .map_err(|e| e.to_string())?;
        let expected: Vec<_> = (0..rows)
            .filter(|n| (n + generation) % BUCKETS == bucket)
            .map(key)
            .collect();
        if found != expected {
            return Err(format!("index bucket {bucket} differs"));
        }
    }
    store.check_indexes_at(snap)?;
    Ok(())
}
fn nanos(start: Instant) -> f64 {
    start.elapsed().as_nanos().max(1) as f64
}
fn add(samples: &mut BTreeMap<String, Vec<f64>>, name: &str, total_ns: f64, operations: u64) {
    samples
        .entry(name.into())
        .or_default()
        .push(total_ns / operations as f64);
}
fn wal_size(path: &Path) -> Result<(u64, u64), String> {
    let mut bytes = 0;
    let mut segments = 0;
    for entry in std::fs::read_dir(path.join("wal")).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().extension().is_some_and(|ext| ext == "seg") {
            bytes += entry.metadata().map_err(|e| e.to_string())?.len();
            segments += 1;
        }
    }
    Ok((bytes, segments))
}

fn measure(options: &Options) -> Result<(Results, Value), String> {
    let scratch = Scratch::new()?;
    let mut samples = BTreeMap::<String, Vec<f64>>::new();
    let keys: Vec<_> = (0..options.rows).map(key).collect();
    for sample in 0..options.samples {
        let path = scratch.0.join(format!("reads-{sample}"));
        let mut store = prepare(&path, options.rows, 4)?;
        let snap = store.snapshot();
        let start = Instant::now();
        for n in 0..options.reads {
            let key = keys
                .get((n % options.rows) as usize)
                .ok_or("missing prepared key")?;
            black_box(
                store
                    .get_at(snap, TABLE, key, &lattice_engines::limits::AllowAll)
                    .map_err(|e| e.to_string())?,
            );
        }
        add(&mut samples, "row_point_read", nanos(start), options.reads);
        let lo = key(0);
        let hi = key(16.min(options.rows));
        let start = Instant::now();
        for _ in 0..options.reads {
            black_box(
                store
                    .scan_at(
                        snap,
                        TABLE,
                        lo.clone()..hi.clone(),
                        &lattice_engines::limits::AllowAll,
                    )
                    .map_err(|e| e.to_string())?,
            );
        }
        add(
            &mut samples,
            "row_range_scan_up_to_16",
            nanos(start),
            options.reads,
        );
        let buckets: Vec<_> = (0..BUCKETS).map(u64::to_le_bytes).collect();
        let start = Instant::now();
        for n in 0..options.reads {
            let bucket = buckets
                .get((n % BUCKETS) as usize)
                .ok_or("missing prepared bucket")?;
            black_box(
                store
                    .lookup_at(snap, INDEX, bucket, &lattice_engines::limits::AllowAll)
                    .map_err(|e| e.to_string())?,
            );
        }
        add(
            &mut samples,
            "row_index_lookup",
            nanos(start),
            options.reads,
        );
        // Construct each transaction before starting its individual durable-commit timer.
        let mut commit_ns = 0.0;
        for n in options.rows..options.rows + COMMITS {
            let mut tx = store.begin();
            tx.put((TABLE, key(n)), value(n, 3));
            let start = Instant::now();
            let committed = store.commit(tx).map_err(|e| e.to_string());
            commit_ns += nanos(start);
            committed?;
        }
        add(&mut samples, "row_commit_one", commit_ns, COMMITS);
        verify(&store, options.rows + COMMITS, 3)?;
    }
    let mut recovery = Vec::new();
    for versions in [1, 4, 16] {
        let path = scratch.0.join(format!("recovery-{versions}"));
        drop(prepare(&path, options.rows, versions)?);
        let (bytes, segments) = wal_size(&path)?;
        recovery.push(json!({"versions_per_key":versions,"rows":options.rows,"row_versions":options.rows*versions,
            "acknowledged_changesets":versions,"wal_bytes":bytes,"segments":segments}));
        for _ in 0..options.samples {
            let start = Instant::now();
            let store = open(&path);
            let ns = nanos(start);
            let store = store?;
            add(
                &mut samples,
                &format!("row_reopen_{versions}_versions"),
                ns,
                1,
            );
            verify(&store, options.rows, versions - 1)?;
        }
    }
    let mut vacuum = Vec::new();
    for sample in 0..options.samples {
        let path = scratch.0.join(format!("vacuum-{sample}"));
        let mut store = prepare(&path, options.rows, 16)?;
        let applied = store.applied_lsn();
        let start = Instant::now();
        let stats = store.vacuum(applied);
        let ns = nanos(start);
        let removed = options.rows * 15;
        if stats.row_versions as u64 != removed || stats.horizon != applied {
            return Err("vacuum removed an unexpected version count".into());
        }
        // Each row visited all sixteen buckets: fifteen obsolete put/delete pairs disappear.
        if stats.index_versions as u64 != options.rows * 30 {
            return Err("vacuum removed an unexpected index version count".into());
        }
        verify(&store, options.rows, 15)?;
        if !matches!(
            store.get_at(
                Snapshot { as_of: 0 },
                TABLE,
                &key(0),
                &lattice_engines::limits::AllowAll
            ),
            Err(RowError::SnapshotTooOld { .. })
        ) {
            return Err("vacuum did not refuse an expired read".into());
        }
        add(&mut samples, "row_vacuum_removed_version", ns, removed);
        vacuum.push(json!({"sample":sample,"row_versions_removed":stats.row_versions,"index_versions_removed":stats.index_versions,
                           "horizon":stats.horizon}));
    }
    let raw = samples.clone();
    let mut results = BTreeMap::new();
    for (name, mut values) in samples {
        values.sort_by(f64::total_cmp);
        let middle = values.len() / 2;
        let upper = *values.get(middle).ok_or("no benchmark samples")?;
        let median = if values.len().is_multiple_of(2) {
            let lower = *values
                .get(middle.checked_sub(1).ok_or("no benchmark samples")?)
                .ok_or("no benchmark samples")?;
            lower / 2.0 + upper / 2.0
        } else {
            upper
        };
        results.insert(name, median);
    }
    Ok((
        results,
        json!({"metadata":options.metadata(),"recovery_datasets":recovery,"vacuum_samples":vacuum,
                       "ns_per_operation_samples":raw}),
    ))
}

fn baseline_paths(profile: &str) -> Result<(PathBuf, PathBuf), String> {
    // Resolve the invoking worktree, not the path embedded in a binary from a shared Cargo cache.
    let current = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = current
        .ancestors()
        .find(|path| {
            path.join("Cargo.toml").is_file()
                && path.join("crates/lattice-bench/Cargo.toml").is_file()
                && path.join("benches/baselines").is_dir()
        })
        .ok_or("run row_bench inside the Lattice workspace")?;
    let dir = root.join("benches/baselines/local");
    Ok((
        dir.join(format!("row-{profile}.json")),
        dir.join(format!("row-{profile}-metadata.json")),
    ))
}
fn run(options: Options) -> Result<bool, String> {
    let (baseline_path, metadata_path) = baseline_paths(&options.profile)?;
    // Reject an absent/mismatched baseline before doing potentially expensive measurement work.
    let baseline = if options.write {
        None
    } else {
        let metadata: Value = serde_json::from_str(
            &std::fs::read_to_string(&metadata_path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if metadata != options.metadata() {
            return Err("baseline workload/platform metadata differs; use its original configuration or record a new local baseline".into());
        }
        Some(parse_results(
            &std::fs::read_to_string(&baseline_path).map_err(|e| e.to_string())?,
        )?)
    };
    let (current, mut raw) = measure(&options)?;
    for (name, ns) in &current {
        println!("{name:<36} {ns:>12.1} ns/op");
    }
    let report_path = baseline_path.with_file_name(format!("row-{}-latest.json", options.profile));
    let parent = baseline_path.parent().ok_or("baseline has no directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let passed = if let Some(baseline) = baseline {
        if baseline.keys().ne(current.keys()) {
            return Err("baseline benchmark set differs; record a new local baseline".into());
        }
        let regressions = compare(&baseline, &current, options.tolerance)?;
        for regression in &regressions {
            println!(
                "REGRESSION {}: {:+.1}%",
                regression.name,
                regression.slowdown() * 100.0
            );
        }
        let passed = regressions.is_empty();
        raw.as_object_mut().ok_or("benchmark report is not an object")?.insert("comparison".into(), json!({"status":if passed { "passed" } else { "failed" },"tolerance":options.tolerance,
                                   "baseline":baseline,"current":current}));
        println!(
            "{}",
            if passed {
                "comparison passed"
            } else {
                "comparison failed"
            }
        );
        passed
    } else {
        std::fs::write(&baseline_path, render_results(&current)).map_err(|e| e.to_string())?;
        std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&options.metadata()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        raw.as_object_mut()
            .ok_or("benchmark report is not an object")?
            .insert(
                "comparison".into(),
                json!({"status":"baseline_recorded_without_comparison"}),
            );
        println!("local baseline written to {}", baseline_path.display());
        true
    };
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&raw).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("raw current samples: {}", report_path.display());
    Ok(passed)
}

fn main() -> std::process::ExitCode {
    match Options::parse(std::env::args().skip(1)).and_then(run) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("row benchmark: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn options_reject_path_escape_empty_workloads_and_invalid_tolerances() {
        for args in [
            vec!["--profile", "../other"],
            vec!["--rows", "0"],
            vec!["--reads", "0"],
            vec!["--samples", "0"],
            vec!["--tolerance", "NaN"],
            vec!["--tolerance", "-1"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }
    #[test]
    fn tiny_workload_produces_all_cases_and_audited_recovery_sizes() {
        let options = Options::parse(
            ["--rows", "17", "--reads", "8", "--samples", "1"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let (results, raw) = measure(&options).unwrap();
        assert_eq!(results.len(), 8);
        assert!(results.values().all(|n| n.is_finite() && *n > 0.0));
        let datasets = raw["recovery_datasets"].as_array().unwrap();
        assert_eq!(datasets.len(), 3);
        assert!(
            datasets
                .windows(2)
                .all(|w| w[0]["wal_bytes"].as_u64().unwrap() < w[1]["wal_bytes"].as_u64().unwrap())
        );
        assert_eq!(raw["vacuum_samples"][0]["row_versions_removed"], 255);
    }
}
