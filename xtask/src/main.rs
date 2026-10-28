//! Repository automation for Lattice: `cargo xtask <command>`.
//!
//! Phase 0 commands: ci, fmt, lint, test, deny, docs, setup, commit-lint, lint-conventions and the
//! structural checks. Later phases fill in sim, fuzz, bench, eval, scorecard, doctor and release.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod backlog;
mod checks;
mod conventions;
mod lex;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Repository root (the parent of the xtask crate).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    println!("+ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(repo_root())
        .status()
        .map_err(|e| format!("failed to start {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed", args.join(" ")))
    }
}

/// In CI (or with XTASK_STRICT=1) a missing optional tool is an error instead of a warning.
fn strict() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os("XTASK_STRICT").is_some()
}

fn tool_available(program: &str, probe: &[&str]) -> bool {
    Command::new(program)
        .args(probe)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn deny() -> Result<(), String> {
    if tool_available("cargo", &["deny", "--version"]) {
        run("cargo", &["deny", "check", "--hide-inclusion-graph"])
    } else if strict() {
        Err("cargo-deny is required in CI (install with taiki-e/install-action)".into())
    } else {
        println!("warning: cargo-deny not installed; skipping (CI enforces it)");
        Ok(())
    }
}

fn docs() -> Result<(), String> {
    if tool_available("python3", &["--version"]) {
        run("python3", &["docs/build.py"])?;
        for page in [
            "index.html",
            "engineering.html",
            "spec.html",
            "reference.html",
        ] {
            if !repo_root().join("docs/site").join(page).exists() {
                return Err(format!("docs build did not produce {page}"));
            }
        }
        Ok(())
    } else if strict() {
        Err("python3 is required to build the docs".into())
    } else {
        println!("warning: python3 not found; skipping docs build");
        Ok(())
    }
}

fn setup() -> Result<(), String> {
    run("git", &["config", "core.hooksPath", ".githooks"])?;
    println!(
        "hooks installed (.githooks). Install `typos` for the typo check: cargo install typos-cli"
    );
    Ok(())
}

fn ci() -> Result<(), String> {
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(
        "cargo",
        &[
            "clippy",
            "-p",
            "latticed",
            "--features",
            "intel",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run("cargo", &["test", "--workspace"])?;
    run("cargo", &["build", "-p", "latticed"])?;
    run("cargo", &["build", "-p", "latticed", "--features", "intel"])?;
    checks::check_layers()?;
    checks::check_unsafe()?;
    checks::check_allow()?;
    checks::check_format_corpus()?;
    checks::check_config_formats()?;
    checks::check_links()?;
    checks::check_quarantine()?;
    conventions::lint_conventions()?;
    backlog::lint()?;
    backlog::adr_lint()?;
    backlog::rfc_lint()?;
    docs()?;
    deny()?;
    println!("ci: all checks passed");
    Ok(())
}

/// Deterministic simulation: `cargo xtask sim [--seeds N] [--steps N] [--from S] [--seed S --profile P]`.
fn sim(args: &[&str]) -> Result<(), String> {
    let mut a: Vec<&str> = vec![
        "run",
        "-q",
        "-p",
        "lattice-testkit",
        "--release",
        "--bin",
        "sim_runner",
        "--",
    ];
    a.extend_from_slice(args);
    run("cargo", &a)
}

/// Benchmarks: `cargo xtask bench [--profile NAME] [--tolerance F] [--write-baseline]`.
/// Prints the runs recorded in a results ledger (default `target/ledger`). Record runs with
/// `cargo xtask sim --ledger target/ledger`; the directory must exist.
fn scorecard(args: &[&str]) -> Result<(), String> {
    let dir = args.first().copied().unwrap_or("target/ledger");
    run(
        "cargo",
        &[
            "run",
            "-q",
            "-p",
            "lattice-cli",
            "--",
            "ledger",
            "show",
            "--dir",
            dir,
        ],
    )
}

fn bench(args: &[&str]) -> Result<(), String> {
    let mut a: Vec<&str> = vec![
        "run",
        "-q",
        "-p",
        "lattice-bench",
        "--release",
        "--bin",
        "bench_runner",
        "--",
    ];
    a.extend_from_slice(args);
    run("cargo", &a)
}

fn not_implemented(name: &str) -> Result<(), String> {
    Err(format!("`{name}` is not implemented in this phase"))
}

fn dispatch(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match cmd {
        "ci" => ci(),
        "fmt" => run("cargo", &["fmt", "--all"]),
        "lint" => run(
            "cargo",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        "test" => run("cargo", &["test", "--workspace"]),
        "deny" => deny(),
        "docs" => docs(),
        "setup" => setup(),
        "check-layers" => checks::check_layers(),
        "check-unsafe" => checks::check_unsafe(),
        "check-allow" => checks::check_allow(),
        "check-format-corpus" => checks::check_format_corpus(),
        "check-config-formats" => checks::check_config_formats(),
        "check-links" => checks::check_links(),
        "check-quarantine" => checks::check_quarantine(),
        "lint-conventions" => conventions::lint_conventions(),
        "commit-lint" => match rest.first() {
            Some(file) => conventions::commit_lint(file),
            None => Err("usage: cargo xtask commit-lint <message-file>".into()),
        },
        "backlog" => match rest.first().copied() {
            Some("seed") => backlog::seed(),
            Some("lint") => backlog::lint(),
            _ => Err("usage: cargo xtask backlog <seed|lint>".into()),
        },
        "adr" => match rest.first().copied() {
            Some("lint") => backlog::adr_lint(),
            Some("index") => backlog::adr_index(),
            _ => Err("usage: cargo xtask adr <lint|index>".into()),
        },
        "rfc" => match rest.first().copied() {
            Some("lint") => backlog::rfc_lint(),
            _ => Err("usage: cargo xtask rfc lint".into()),
        },
        "traceability" => checks::traceability(),
        "sim" => sim(&rest),
        "bench" => bench(&rest),
        "scorecard" => scorecard(&rest),
        "fuzz" | "eval" | "doctor" | "release" | "gen" | "gate" => not_implemented(cmd),
        "help" | "--help" | "-h" => {
            println!(
                "commands: ci fmt lint test deny docs setup commit-lint lint-conventions \
                 check-layers check-unsafe check-allow check-format-corpus check-config-formats \
                 check-links check-quarantine backlog adr rfc traceability \
                 sim fuzz bench eval scorecard doctor release gate"
            );
            Ok(())
        }
        other => Err(format!(
            "unknown command `{other}` (try `cargo xtask help`)"
        )),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
