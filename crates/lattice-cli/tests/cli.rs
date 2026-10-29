//! The `lattice` program through its real binary: argument parsing and exit codes are what scripts
//! and CI depend on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use lattice_core::changeset::ChangeSet;
use lattice_core::{Config, Kernel, ledger};
use lattice_seams::{OsRng, StdIo, SystemClock};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

// A fork can temporarily inherit another test thread's open WAL lock. Keep child creation
// and in-process ledger writes apart, so the child cannot prolong a released writer lock.
static PROCESS_IO: Mutex<()> = Mutex::new(());

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lattice-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lattice(args: &[&str]) -> Output {
    let _guard = PROCESS_IO.lock().unwrap();
    Command::new(env!("CARGO_BIN_EXE_lattice"))
        .args(args)
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn data_dir(name: &str, commits: usize) -> PathBuf {
    let dir = workdir(name);
    let config = Config {
        dir: "wal".into(),
        ..Config::default()
    };
    let (mut k, _) = Kernel::open(StdIo::new(&dir), SystemClock, OsRng, &config).unwrap();
    for i in 0..commits {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i as u8; 20])).unwrap();
    }
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p.to_string_lossy().into_owned()
}

const V1: &str = "type Ticket {\n  title: text\n}\n";
const V2: &str = "type Ticket {\n  title: text\n  owner: text\n}\n";

#[test]
fn version_has_three_spellings() {
    for flag in ["--version", "-V", "version"] {
        let o = lattice(&[flag]);
        assert_eq!(code(&o), 0);
        assert!(out(&o).starts_with("lattice 0."), "{}", out(&o));
    }
}

#[test]
fn an_unknown_command_prints_usage_and_fails() {
    let o = lattice(&["frobnicate"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("usage: lattice doctor"));
    assert_eq!(code(&lattice(&[])), 2);
    // `ledger` needs the `show` subcommand.
    assert_eq!(code(&lattice(&["ledger", "list"])), 2);
}

#[test]
fn doctor_reports_a_healthy_log_and_a_damaged_one() {
    let dir = data_dir("doctor", 3);
    let d = dir.to_str().unwrap();
    let o = lattice(&["doctor", "--dir", d]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(out(&o).contains("3 change sets"), "{}", out(&o));
    assert!(out(&o).contains("result: healthy"));
    // A different log name is a different log.
    let o = lattice(&["doctor", "--dir", d, "--wal", "other"]);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("cannot read the log"), "{}", out(&o));

    // Damage the middle of the only segment: that is not a torn tail.
    let seg = std::fs::read_dir(dir.join("wal"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "seg"))
        .unwrap();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&seg, bytes).unwrap();
    let o = lattice(&["doctor", "--dir", d]);
    assert_eq!(code(&o), 1);
    assert!(
        out(&o).contains("result: damaged") || out(&o).contains("PROBLEM"),
        "{}",
        out(&o)
    );
}

#[test]
fn ledger_show_lists_entries_and_counts_them() {
    let dir = workdir("ledger");
    ledger_entry(&dir, "sim", "{\"runs\":1}");
    ledger_entry(&dir, "bench", "{\"x\":2}");
    let o = lattice(&["ledger", "show", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let text = out(&o);
    assert!(
        text.contains("sim") && text.contains("{\"runs\":1}"),
        "{text}"
    );
    assert!(
        text.contains("bench") && text.ends_with("2 entries\n"),
        "{text}"
    );
}

#[test]
fn lsl_check_accepts_and_rejects() {
    let dir = workdir("check");
    let ok = write(&dir, "ok.lsl", V1);
    let o = lattice(&["lsl", "check", &ok]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).starts_with("ok: 1 types"), "{}", out(&o));
    let bad = write(&dir, "bad.lsl", "type Ticket {\n  title: nope\n}\n");
    let o = lattice(&["lsl", "check", &bad]);
    assert_eq!(code(&o), 1);
    assert!(!err(&o).is_empty());
    let o = lattice(&["lsl", "check", "/no/such/file.lsl"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("/no/such/file.lsl"));
}

#[test]
fn lsl_fmt_prints_checks_and_writes() {
    let dir = workdir("fmt");
    let messy = write(&dir, "m.lsl", "type   Ticket{ title:text }\n");
    let printed = lattice(&["lsl", "fmt", &messy]);
    assert_eq!(code(&printed), 0);
    assert!(out(&printed).contains("type Ticket {"), "{}", out(&printed));
    assert_eq!(
        std::fs::read_to_string(&messy).unwrap(),
        "type   Ticket{ title:text }\n"
    );
    // --check fails for unformatted text and passes once it is written.
    assert_eq!(code(&lattice(&["lsl", "fmt", &messy, "--check"])), 1);
    let o = lattice(&["lsl", "fmt", &messy, "--write"]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).is_empty());
    assert_eq!(std::fs::read_to_string(&messy).unwrap(), out(&printed));
    assert_eq!(code(&lattice(&["lsl", "fmt", &messy, "--check"])), 0);
}

#[test]
fn lsl_compat_exit_code_follows_backward_compatibility() {
    let dir = workdir("compat");
    let v1 = write(&dir, "v1.lsl", V1);
    let v2 = write(&dir, "v2.lsl", V2);
    let o = lattice(&["lsl", "compat", &v1, &v1]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).contains("backward compatible (old data stays valid): true"));
    let o = lattice(&["lsl", "compat", &v1, &v2]);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("owner"), "{}", out(&o));
}

#[test]
fn lsl_with_the_wrong_arguments_prints_usage_and_exits_two() {
    let o = lattice(&["lsl"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("usage: lattice lsl"));
    assert_eq!(code(&lattice(&["lsl", "check"])), 2);
    assert_eq!(code(&lattice(&["lsl", "compat", "only-one"])), 2);
    assert_eq!(code(&lattice(&["lsl", "wibble", "x"])), 2);
}

fn ledger_entry(dir: &Path, kind: &str, json: &str) {
    let _guard = PROCESS_IO.lock().unwrap();
    ledger::append(StdIo::new(dir), SystemClock, OsRng, kind, json).unwrap();
}

fn sim_entry(dir: &Path, runs: u64, failing: u64, incomplete: u64) {
    let json = format!(
        "{{\"runs\":{runs},\"failing\":{failing},\"cut_short\":0,\"incomplete\":{incomplete}}}"
    );
    ledger_entry(dir, "sim", &json);
}

#[test]
fn ledger_check_passes_a_clean_latest_run_and_fails_each_kind_of_bad_one() {
    let check = |dir: &Path| lattice(&["ledger", "check", "--dir", dir.to_str().unwrap()]);

    let dir = workdir("check-ok");
    sim_entry(&dir, 10, 0, 0);
    sim_entry(&dir, 10, 0, 5); // exactly half unverified is still acceptable
    let o = check(&dir);
    assert_eq!(code(&o), 0, "{}", out(&o));
    assert!(
        out(&o).contains("2 clean simulation run(s) in a row"),
        "{}",
        out(&o)
    );

    let dir = workdir("check-violations");
    sim_entry(&dir, 10, 1, 0);
    let o = check(&dir);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("the latest run has violations"));

    let dir = workdir("check-empty-run");
    sim_entry(&dir, 0, 0, 0);
    let o = check(&dir);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("verified nothing"));
    assert!(
        out(&o).contains("0 clean simulation run(s) in a row"),
        "{}",
        out(&o)
    );

    let dir = workdir("check-unverified");
    sim_entry(&dir, 10, 0, 6);
    let o = check(&dir);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("more than half"));

    // A failing run breaks the streak that ends at the latest clean one.
    let dir = workdir("check-streak");
    sim_entry(&dir, 10, 0, 0);
    sim_entry(&dir, 10, 2, 0);
    sim_entry(&dir, 10, 0, 0);
    sim_entry(&dir, 10, 0, 0);
    let o = check(&dir);
    assert_eq!(code(&o), 0);
    assert!(
        out(&o).contains("2 clean simulation run(s) in a row"),
        "{}",
        out(&o)
    );

    let dir = workdir("check-none");
    let o = check(&dir);
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("no simulation run in the ledger"));
}

#[test]
fn validated_results_round_trip_and_reject_oversized_or_nonobject_input() {
    let dir = workdir("record-export");
    let file = write(
        &dir,
        "result.json",
        "{\"runs\":10,\"failing\":0,\"incomplete\":0}",
    );
    let args = [
        "ledger",
        "record",
        "--dir",
        dir.to_str().unwrap(),
        "--kind",
        "sim",
        "--json",
        &file,
    ];
    assert_eq!(code(&lattice(&args)), 0);
    let exported = lattice(&["ledger", "export", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&exported), 0);
    let entries: serde_json::Value = serde_json::from_str(&out(&exported)).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["kind"], "sim");
    let value: serde_json::Value =
        serde_json::from_str(entries[0]["json"].as_str().unwrap()).unwrap();
    assert_eq!(value["runs"], 10);
    for bad in ["[]".to_string(), "{".to_string(), " ".repeat(65537)] {
        std::fs::write(&file, bad).unwrap();
        assert_eq!(code(&lattice(&args)), 1);
    }
    let exported = lattice(&["ledger", "export", "--dir", dir.to_str().unwrap()]);
    let entries: serde_json::Value = serde_json::from_str(&out(&exported)).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
}
