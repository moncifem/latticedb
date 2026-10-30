//! The `sim-runner` command line as a function: argument handling, the per-profile loop and the
//! report, with the ledger write and the printing left to the caller so it can be tested directly.

use crate::scenario::{profiles, run};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// What a run printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Lines for standard output.
    pub stdout: Vec<String>,
    /// Lines for standard error.
    pub stderr: Vec<String>,
    /// The JSON summary to append to the ledger when `--ledger DIR` was given.
    pub ledger: Option<(String, String)>,
    /// Process exit code: 0 clean, 1 violations, 2 bad arguments.
    pub code: u8,
}

/// Runs the simulator and collects everything it prints. `args` excludes the program name.
pub fn execute(args: &[String]) -> Report {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (code, ledger) = execute_with(args, &mut |to_stderr, line| {
        if to_stderr {
            stderr.push(line.to_string())
        } else {
            stdout.push(line.to_string())
        }
    });
    Report {
        stdout,
        stderr,
        ledger,
        code,
    }
}

/// Runs the simulator as the command line describes, handing each output line to `sink` as soon as it
/// exists (`true` for standard error). Returns the exit code and the ledger entry to append.
pub fn execute_with(
    args: &[String],
    sink: &mut dyn FnMut(bool, &str),
) -> (u8, Option<(String, String)>) {
    let seeds: u64 = arg(args, "--seeds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let steps: usize = arg(args, "--steps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let from: u64 = arg(args, "--from")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let only_profile = arg(args, "--profile");
    let ledger = arg(args, "--ledger");
    let single = arg(args, "--seed").and_then(|v| v.parse::<u64>().ok());

    if only_profile
        .as_deref()
        .is_some_and(|p| !profiles().iter().any(|(name, _)| *name == p))
    {
        sink(
            true,
            &format!("unknown profile: {}", only_profile.unwrap_or_default()),
        );
        return (2, None);
    }
    let range: Vec<u64> = match single {
        Some(s) => vec![s],
        None => (from..from + seeds).collect(),
    };
    if range.is_empty() {
        sink(true, "nothing to run: --seeds must be at least 1");
        return (2, None);
    }
    let (mut runs, mut failures, mut cut_short, mut incomplete, mut cuts, mut recoveries) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for (name, profile) in profiles() {
        if only_profile.as_deref().is_some_and(|p| p != name) {
            continue;
        }
        let (mut p_fail, mut p_cut, mut p_inc) = (0u64, 0u64, 0u64);
        for &seed in &range {
            let out = run(seed, profile, steps, 2048);
            runs += 1;
            cuts += out.power_cuts;
            recoveries += u64::from(out.recoveries);
            p_cut += u64::from(out.cut_short);
            p_inc += u64::from(out.incomplete);
            if !out.violations.is_empty() {
                p_fail += 1;
                sink(
                    false,
                    &format!(
                        "VIOLATION profile={name} seed={seed}: {:?}",
                        out.violations.first()
                    ),
                );
                sink(
                    false,
                    &format!("  reproduce: cargo xtask sim --seed {seed} --profile {name}"),
                );
            }
        }
        // A run that could not be verified is not evidence; too many of them fail the profile.
        if p_inc * 2 > range.len() as u64 {
            sink(
                false,
                &format!("profile {name}: more than half of the runs were not verified"),
            );
            p_fail += 1;
        }
        failures += p_fail;
        cut_short += p_cut;
        incomplete += p_inc;
        sink(
            false,
            &format!(
                "profile {name}: {} seeds, {p_fail} failing, {p_cut} cut short (still verified), {p_inc} incomplete (not verified)",
                range.len()
            ),
        );
    }
    sink(
        false,
        &format!(
            "total: {runs} runs, {failures} failing, {cut_short} cut short, {incomplete} incomplete; \
             {recoveries} recoveries, {cuts} power cuts inside durability protocols"
        ),
    );
    let entry = ledger.map(|dir| {
        let json = format!(
            "{{\"runs\":{runs},\"failing\":{failures},\"cut_short\":{cut_short},\"incomplete\":{incomplete},\"recoveries\":{recoveries},\"power_cuts\":{cuts},\"seeds\":{},\"steps\":{steps}}}",
            range.len()
        );
        (dir, json)
    });
    (u8::from(failures != 0), entry)
}
