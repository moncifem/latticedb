//! The simulator command line as a function: argument defaults and parsing, the exit codes, the
//! per-profile report and the ledger summary. Mutation testing showed the counters and the argument
//! lookup were not pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::sim_cli::{Report, execute};

fn run(args: &[&str]) -> Report {
    execute(&args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
}

fn profile_lines(report: &Report) -> Vec<&String> {
    report
        .stdout
        .iter()
        .filter(|l| l.starts_with("profile "))
        .collect()
}

#[test]
fn a_single_seed_runs_every_profile_once_and_reports_each() {
    let r = run(&["--seed", "5", "--steps", "40"]);
    assert_eq!(r.code, 0, "{:?}", r.stdout);
    assert!(r.stderr.is_empty());
    let names: Vec<&str> = profile_lines(&r)
        .iter()
        .map(|l| l.split_whitespace().nth(1).unwrap().trim_end_matches(':'))
        .collect();
    assert_eq!(
        names,
        [
            "clean",
            "crash-heavy",
            "disk-faulty",
            "hostile",
            "everything"
        ]
    );
    for line in profile_lines(&r) {
        assert!(line.contains(": 1 seeds, 0 failing,"), "{line}");
    }
    let total = r.stdout.last().unwrap();
    assert!(total.starts_with("total: 5 runs, 0 failing,"), "{total}");
}

#[test]
fn seeds_from_and_profile_select_exactly_the_runs_asked_for() {
    let r = run(&[
        "--seeds",
        "3",
        "--from",
        "10",
        "--steps",
        "30",
        "--profile",
        "hostile",
    ]);
    assert_eq!(r.code, 0);
    assert_eq!(profile_lines(&r).len(), 1);
    assert!(profile_lines(&r)[0].starts_with("profile hostile: 3 seeds, 0 failing,"));
    assert!(
        r.stdout
            .last()
            .unwrap()
            .starts_with("total: 3 runs, 0 failing,")
    );
    // A single seed overrides the range.
    let one = run(&[
        "--seeds",
        "9",
        "--seed",
        "4",
        "--steps",
        "30",
        "--profile",
        "clean",
    ]);
    assert!(one.stdout.last().unwrap().starts_with("total: 1 runs,"));
}

#[test]
fn the_same_arguments_give_the_same_report() {
    let args = ["--seeds", "2", "--steps", "40", "--profile", "everything"];
    assert_eq!(run(&args), run(&args));
    let other = run(&[
        "--seeds",
        "2",
        "--from",
        "100",
        "--steps",
        "40",
        "--profile",
        "everything",
    ]);
    assert_ne!(run(&args).stdout.last(), other.stdout.last());
}

#[test]
fn bad_arguments_exit_with_two_and_say_why() {
    let unknown = run(&["--profile", "nope"]);
    assert_eq!(unknown.code, 2);
    assert_eq!(unknown.stderr, vec!["unknown profile: nope".to_string()]);
    assert!(unknown.stdout.is_empty());
    let none = run(&["--seeds", "0"]);
    assert_eq!(none.code, 2);
    assert_eq!(
        none.stderr,
        vec!["nothing to run: --seeds must be at least 1".to_string()]
    );
}

#[test]
fn an_option_without_a_usable_value_falls_back_to_its_default() {
    // A missing or unparsable value means the default (1000 seeds would be slow, so check --seed's
    // neighbours instead): the steps value is ignored and the run still completes.
    let r = run(&["--seed", "2", "--profile", "clean", "--steps", "x"]);
    assert_eq!(r.code, 0);
    let last = run(&["--seed", "2", "--profile", "clean", "--steps"]);
    assert_eq!(last.code, 0);
    assert_eq!(r.stdout.last(), last.stdout.last());
}

#[test]
fn the_ledger_entry_carries_the_counts_and_the_run_shape() {
    let r = run(&[
        "--seeds",
        "2",
        "--steps",
        "40",
        "--profile",
        "clean",
        "--ledger",
        "/some/dir",
    ]);
    let (dir, json) = r.ledger.clone().unwrap();
    assert_eq!(dir, "/some/dir");
    assert!(
        json.starts_with("{\"runs\":2,\"failing\":0,\"cut_short\":"),
        "{json}"
    );
    assert!(json.ends_with(",\"seeds\":2,\"steps\":40}"), "{json}");
    assert!(
        run(&["--seed", "1", "--steps", "10", "--profile", "clean"])
            .ledger
            .is_none()
    );
}

#[test]
fn counters_match_recorded_runs() {
    // Recorded from the simulator, which is deterministic: a run with an unverifiable (incomplete) seed,
    // and one with a verified-but-cut-short seed. They pin every counter the report sums.
    let incomplete = run(&["--seeds", "8", "--steps", "150", "--profile", "everything"]);
    assert_eq!(
        incomplete.stdout,
        vec![
            "profile everything: 8 seeds, 0 failing, 0 cut short (still verified), 1 incomplete (not verified)".to_string(),
            "total: 8 runs, 0 failing, 0 cut short, 1 incomplete; 475 recoveries, 164 power cuts inside durability protocols".to_string(),
        ]
    );
    let cut_short = run(&["--seeds", "100", "--steps", "150", "--profile", "hostile"]);
    assert_eq!(
        cut_short.stdout,
        vec![
            "profile hostile: 100 seeds, 0 failing, 1 cut short (still verified), 0 incomplete (not verified)".to_string(),
            "total: 100 runs, 0 failing, 1 cut short, 0 incomplete; 3327 recoveries, 1901 power cuts inside durability protocols".to_string(),
        ]
    );
}

#[test]
fn a_profile_where_more_than_half_the_runs_are_unverified_fails() {
    // Under the `everything` profile seed 6 is incomplete (the run could not be verified).
    let all = run(&["--seed", "6", "--steps", "150", "--profile", "everything"]);
    assert_eq!(all.code, 1);
    assert_eq!(
        all.stdout,
        vec![
            "profile everything: more than half of the runs were not verified".to_string(),
            "profile everything: 1 seeds, 1 failing, 0 cut short (still verified), 1 incomplete (not verified)".to_string(),
            "total: 1 runs, 1 failing, 0 cut short, 1 incomplete; 39 recoveries, 21 power cuts inside durability protocols".to_string(),
        ]
    );
    // Exactly half unverified (seeds 5 and 6) is not more than half.
    let half = run(&[
        "--from",
        "5",
        "--seeds",
        "2",
        "--steps",
        "150",
        "--profile",
        "everything",
    ]);
    assert_eq!(half.code, 0, "{:?}", half.stdout);
    assert!(
        half.stdout[0].contains("2 seeds, 0 failing,") && half.stdout[0].contains("1 incomplete")
    );
    // A profile's failures add up across profiles into the total.
    let several = run(&["--seed", "6", "--steps", "150"]);
    assert!(
        several.stdout.last().unwrap().contains(", 1 failing,"),
        "{:?}",
        several.stdout
    );
    assert_eq!(several.code, 1);
}
