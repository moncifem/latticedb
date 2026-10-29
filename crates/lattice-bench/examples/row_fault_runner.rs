//! Host-only command-line boundary for the exact row-fault oracle used by engine tests.
//! No date, environment or argument reads enter that deterministic oracle.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

#[path = "../../lattice-engines/tests/row_faults.rs"]
mod row_faults;

use lattice_sim::FaultProfile;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const PROFILES: [&str; 5] = [
    "clean",
    "crash-heavy",
    "disk-faulty",
    "hostile",
    "everything",
];
const SHARDS: u64 = 8;
const SEEDS_PER_SHARD: u64 = 25_000;
const STEPS: u64 = 150;

#[derive(Debug)]
struct Options {
    day: u64,
    profile: String,
    shard: u64,
    repository: String,
    run: u64,
    attempt: u64,
    revision: String,
    output: PathBuf,
    diagnostic_seeds: Option<u64>,
    plan_only: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut values = BTreeMap::new();
        let mut plan_only = false;
        while let Some(flag) = args.next() {
            if flag == "--plan-only" {
                if plan_only {
                    return Err("duplicate --plan-only".into());
                }
                plan_only = true;
                continue;
            }
            if ![
                "--day",
                "--profile",
                "--shard",
                "--repository",
                "--run",
                "--attempt",
                "--revision",
                "--output",
                "--diagnostic-seeds",
            ]
            .contains(&flag.as_str())
            {
                return Err(format!("unknown argument {flag}"));
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            if values.insert(flag.clone(), value).is_some() {
                return Err(format!("duplicate {flag}"));
            }
        }
        let required = |name: &str| {
            values
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing {name}"))
        };
        let number = |name: &str| {
            required(name)?
                .parse::<u64>()
                .map_err(|_| format!("invalid {name}"))
        };
        let repository = required("--repository")?;
        let components: Vec<_> = repository.split('/').collect();
        if components.len() != 2
            || components.iter().any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        {
            return Err("repository must be owner/name".into());
        }
        let revision = required("--revision")?;
        if revision.len() != 40
            || !revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("revision must be a lowercase full commit SHA".into());
        }
        let run = number("--run")?;
        let attempt = number("--attempt")?;
        if run == 0 || attempt == 0 {
            return Err("run and attempt must be positive".into());
        }
        let diagnostic_seeds = values
            .get("--diagnostic-seeds")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "invalid --diagnostic-seeds".to_string())
            })
            .transpose()?;
        if diagnostic_seeds.is_some_and(|n| !(1..=128).contains(&n)) {
            return Err(
                "diagnostic seed count must be 1..=128; it never counts as nightly volume".into(),
            );
        }
        let output = PathBuf::from(required("--output")?);
        if output.as_os_str().is_empty() {
            return Err("empty output path".into());
        }
        Ok(Self {
            day: number("--day")?,
            profile: required("--profile")?,
            shard: number("--shard")?,
            repository,
            run,
            attempt,
            revision,
            output,
            diagnostic_seeds,
            plan_only,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Selection {
    day: u64,
    profile: String,
    shard: u64,
    from: u64,
    to_exclusive: u64,
    count: u64,
    diagnostic: bool,
}

impl Selection {
    fn new(
        day: u64,
        profile: &str,
        shard: u64,
        diagnostic_seeds: Option<u64>,
    ) -> Result<Self, String> {
        if !PROFILES.contains(&profile) {
            return Err("unknown row fault profile".into());
        }
        if shard >= SHARDS {
            return Err("shard must be 0..8".into());
        }
        if diagnostic_seeds.is_some_and(|n| !(1..=128).contains(&n)) {
            return Err("invalid diagnostic seed count".into());
        }
        let per_profile = SHARDS
            .checked_mul(SEEDS_PER_SHARD)
            .ok_or("seed budget overflow")?;
        let base = day
            .checked_mul(per_profile)
            .ok_or("daily seed base overflow")?;
        // Refuse an incomplete final day even if this particular shard would still fit.
        base.checked_add(per_profile)
            .ok_or("daily seed endpoint overflow")?;
        let from = shard
            .checked_mul(SEEDS_PER_SHARD)
            .and_then(|offset| base.checked_add(offset))
            .ok_or("shard seed base overflow")?;
        let count = diagnostic_seeds.unwrap_or(SEEDS_PER_SHARD);
        let to_exclusive = from
            .checked_add(count)
            .ok_or("shard seed endpoint overflow")?;
        Ok(Self {
            day,
            profile: profile.into(),
            shard,
            from,
            to_exclusive,
            count,
            diagnostic: diagnostic_seeds.is_some(),
        })
    }

    fn fault_profile(&self) -> Result<FaultProfile, String> {
        lattice_testkit::scenario::profiles()
            .into_iter()
            .find(|(name, _)| *name == self.profile)
            .map(|(_, profile)| profile)
            .ok_or_else(|| "compiled row oracle does not contain the selected profile".into())
    }

    fn json(&self) -> Value {
        json!({"utc_day":self.day,"profile":self.profile,"shard":self.shard,"shards_per_profile":SHARDS,
               "from":self.from,"to_exclusive":self.to_exclusive,"planned_observed":self.count,"steps":STEPS,
               "configured_total_histories_per_night":PROFILES.len() as u64 * SHARDS * SEEDS_PER_SHARD,
               "mode":if self.diagnostic {"diagnostic_only"} else {"nightly_volume"}})
    }
}

fn validate_counts(counts: &BTreeMap<&str, u64>, selection: &Selection) -> Result<(), String> {
    let observed = counts
        .get("observed")
        .copied()
        .ok_or("missing terminal observed count")?;
    let verified = counts
        .get("verified")
        .copied()
        .ok_or("missing verified count")?;
    let censored = counts
        .get("rot_censored")
        .copied()
        .ok_or("missing censored count")?;
    let full = counts
        .get("full_step_verified")
        .copied()
        .ok_or("missing full-step count")?;
    let cut_short = counts
        .get("cut_short")
        .copied()
        .ok_or("missing cut-short count")?;
    if observed != selection.count
        || verified.checked_add(censored) != Some(observed)
        || full > verified
        || cut_short > observed
    {
        return Err("terminal counts do not match the exact selected workload".into());
    }
    Ok(())
}

fn execute(options: &Options) -> Result<bool, String> {
    let selection = Selection::new(
        options.day,
        &options.profile,
        options.shard,
        options.diagnostic_seeds,
    )?;
    let profile = selection.fault_profile()?;
    let (status, counts, failure, success) = if options.plan_only {
        ("plan_only", Value::Null, None, true)
    } else {
        match std::panic::catch_unwind(|| {
            row_faults::verify_profile(
                &selection.profile,
                profile,
                selection.from..selection.to_exclusive,
                STEPS,
            )
        }) {
            Ok(counts) => {
                validate_counts(&counts, &selection)?;
                ("completed", json!(counts), None, true)
            }
            Err(_) => (
                "failed",
                Value::Null,
                Some(
                    "row oracle or its fault-exercise assertions failed; stderr contains the profile/seed trace; terminal counts unavailable",
                ),
                false,
            ),
        }
    };
    let document = json!({"schema":1,"source":{"repository":options.repository,"run_id":options.run,
        "attempt":options.attempt,"revision":options.revision,
        "url":format!("https://github.com/{}/actions/runs/{}",options.repository,options.run)},
        "selection":selection.json(),"status":status,"counts":counts,"failure":failure,
        "phase_gate_passed":false,
        "interpretation":"Observed histories include rot-censored and cut-short histories; diagnostics and plans are not nightly volume evidence."});
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.output)
        .map_err(|error| format!("cannot create {}: {error}", options.output.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| error.to_string())?;
    println!("{}", document);
    Ok(success)
}

fn main() -> ExitCode {
    match Options::parse(std::env::args().skip(1)).and_then(|options| execute(&options)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("row-fault runner: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        [
            "--day",
            "20000",
            "--profile",
            "clean",
            "--shard",
            "0",
            "--repository",
            "moncifem/latticedb",
            "--run",
            "1",
            "--attempt",
            "1",
            "--revision",
            "0123456789abcdef0123456789abcdef01234567",
            "--output",
            "result.json",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn fixed_matrix_covers_one_million_profile_seed_pairs_and_adjacent_days_do_not_overlap() {
        assert_eq!(
            PROFILES,
            [
                "clean",
                "crash-heavy",
                "disk-faulty",
                "hostile",
                "everything"
            ]
        );
        assert_eq!((SHARDS, SEEDS_PER_SHARD, STEPS), (8, 25_000, 150));
        for day in [0, 1, 20_000, 365_000] {
            let mut total = 0;
            for name in PROFILES {
                let cases: Vec<_> = (0..8)
                    .map(|shard| Selection::new(day, name, shard, None).unwrap())
                    .collect();
                assert_eq!(cases.len(), 8);
                for (shard, case) in cases.iter().enumerate() {
                    assert_eq!(case.from, day * 200_000 + shard as u64 * 25_000);
                    assert_eq!(
                        case.to_exclusive,
                        day * 200_000 + (shard as u64 + 1) * 25_000
                    );
                    assert_eq!(case.count, 25_000);
                    total += case.count;
                    assert!(case.fault_profile().is_ok());
                }
                assert!(cases.windows(2).all(|pair| pair.first().unwrap().to_exclusive == pair.last().unwrap().from));
                assert_eq!(
                    cases.last().unwrap().to_exclusive,
                    Selection::new(day + 1, name, 0, None).unwrap().from
                );
            }
            assert_eq!(total, 1_000_000);
        }
    }

    #[test]
    fn invalid_selections_overflow_and_missing_terminal_work_are_rejected() {
        assert!(Selection::new(0, "missing", 0, None).is_err());
        assert!(Selection::new(0, "clean", 8, None).is_err());
        assert!(Selection::new(0, "clean", u64::MAX, None).is_err());
        assert!(Selection::new(u64::MAX, "clean", 0, None).is_err());
        assert!(Selection::new(u64::MAX / 200_000, "clean", 0, None).is_err());
        let selection = Selection::new(0, "clean", 0, None).unwrap();
        for observed in [0, 24_999, 25_001] {
            let counts = BTreeMap::from([
                ("observed", observed),
                ("verified", observed),
                ("rot_censored", 0),
                ("full_step_verified", observed),
                ("cut_short", 0),
            ]);
            assert!(validate_counts(&counts, &selection).is_err());
        }
        let counts = BTreeMap::from([
            ("observed", 25_000),
            ("verified", 24_900),
            ("rot_censored", 100),
            ("full_step_verified", 24_850),
            ("cut_short", 50),
        ]);
        assert!(validate_counts(&counts, &selection).is_ok());
        assert!(validate_counts(&BTreeMap::new(), &selection).is_err());
    }

    #[test]
    fn diagnostics_and_plans_cannot_be_confused_with_full_volume() {
        let normal = Selection::new(20_000, "clean", 0, None).unwrap();
        let tiny = Selection::new(20_000, "clean", 0, Some(48)).unwrap();
        assert_eq!(
            (tiny.from, tiny.count, tiny.to_exclusive),
            (normal.from, 48, normal.from + 48)
        );
        assert!(tiny.diagnostic);
        assert_eq!(tiny.json()["mode"], "diagnostic_only");
        assert_eq!(normal.json()["mode"], "nightly_volume");
        assert!(Selection::new(0, "clean", 0, Some(0)).is_err());
        assert!(Selection::new(0, "clean", 0, Some(129)).is_err());
        let mut input = args();
        input.push("--plan-only".into());
        assert!(Options::parse(input).unwrap().plan_only);
    }

    #[test]
    fn a_tiny_explicit_diagnostic_uses_the_real_oracle_and_reports_exact_terminal_counts() {
        let selection = Selection::new(0, "clean", 0, Some(48)).unwrap();
        let counts = row_faults::verify_profile(
            "clean",
            selection.fault_profile().unwrap(),
            selection.from..selection.to_exclusive,
            STEPS,
        );
        assert!(validate_counts(&counts, &selection).is_ok());
        assert_eq!(counts.get("observed"), Some(&48));
        assert_eq!(counts.get("verified"), Some(&48));
        assert_eq!(counts.get("rot_censored"), Some(&0));
        assert!(selection.diagnostic);
    }

    #[test]
    fn command_line_refuses_unknown_duplicate_missing_and_invalid_source_fields() {
        assert!(Options::parse(args()).is_ok());
        let mut unknown = args();
        unknown.push("--skip-checks".into());
        assert!(Options::parse(unknown).is_err());
        let mut duplicate = args();
        duplicate.extend(["--shard".into(), "1".into()]);
        assert!(Options::parse(duplicate).is_err());
        let mut missing = args();
        missing.pop();
        assert!(Options::parse(missing).is_err());
        for (field, value) in [
            ("--run", "0"),
            ("--attempt", "0"),
            ("--revision", "short"),
            ("--repository", "bad/path/extra"),
            ("--day", "-1"),
        ] {
            let mut input = args();
            let index = input.iter().position(|arg| arg == field).unwrap();
            *input.get_mut(index + 1).unwrap() = value.into();
            assert!(Options::parse(input).is_err(), "{field} accepted {value}");
        }
    }

    #[test]
    fn checked_day_endpoints_match_a_wider_integer_model() {
        let mut days = vec![
            0,
            1,
            20_000,
            u64::MAX / 200_000 - 1,
            u64::MAX / 200_000,
            u64::MAX,
        ];
        for bit in 0..64 {
            let boundary = 1u64 << bit;
            days.push(boundary - 1);
            days.push(boundary);
            if let Some(next) = boundary.checked_add(1) {
                days.push(next);
            }
        }
        for day in days {
            for shard in 0u64..8 {
                let end = (u128::from(day) + 1) * 200_000;
                match Selection::new(day, "clean", shard, None) {
                    Ok(selection) => {
                        assert!(end <= u128::from(u64::MAX));
                        assert_eq!(
                            u128::from(selection.from),
                            u128::from(day) * 200_000 + u128::from(shard) * 25_000
                        );
                        assert_eq!(
                            u128::from(selection.to_exclusive),
                            u128::from(day) * 200_000 + (u128::from(shard) + 1) * 25_000
                        );
                    }
                    Err(_) => assert!(end > u128::from(u64::MAX)),
                }
            }
        }
    }
}
