//! An evidence bundle for a phase review. Collecting it never signs a gate.

use crate::repo_root;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Options {
    phase: u8,
    output: PathBuf,
    ledger: Option<PathBuf>,
}

fn options(args: &[&str]) -> Result<Options, String> {
    let mut phase = None;
    let mut output = None;
    let mut ledger = None;
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        let key = pair.first().copied().unwrap_or("");
        let value = pair.get(1).copied().unwrap_or("");
        match key {
            "--phase" if phase.is_none() => {
                phase = Some(value.parse::<u8>().map_err(|_| "phase must be 0 to 12")?);
            }
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--ledger" if ledger.is_none() => ledger = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown or repeated gate option: {key}")),
        }
    }
    if !remainder.is_empty() {
        return Err("gate options need values".into());
    }
    let phase = phase
        .filter(|p| *p <= 12)
        .ok_or("usage: cargo xtask gate --phase <0..12> [--output DIR] [--ledger LEDGER_ROOT]")?;
    Ok(Options {
        phase,
        output: output.unwrap_or_else(|| PathBuf::from(format!("target/gates/phase-{phase}"))),
        ledger,
    })
}

fn observe(root: &Path, program: &str, args: &[&str]) -> Value {
    match Command::new(program).args(args).current_dir(root).output() {
        Ok(out) => json!({
            "program": program,
            "args": args,
            "passed": out.status.success(),
            "exit_code": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }),
        Err(e) => {
            json!({"program": program, "args": args, "passed": false, "error": e.to_string()})
        }
    }
}

/// Extract the declared criteria, retaining their evidence requirements without inferring a pass
/// from prose in a status document or a successful narrow check.
fn criteria(text: &str, phase: u8) -> Vec<Value> {
    let prefix = format!("E{phase}-");
    text.lines()
        .filter_map(|line| {
            let cells: Vec<_> = line
                .trim()
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            let id = *cells.first()?;
            let number = id.strip_prefix(&prefix)?;
            if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(json!({
                "id": id,
                "criterion": cells.get(1).copied().unwrap_or(""),
                "required_evidence": cells.get(2).copied(),
                "review_status": "unverified",
            }))
        })
        .collect()
}

/// Snapshot only WAL segments, never repair the source ledger or copy unrelated user files.
/// This is not an atomic live-writer snapshot; raw bytes and diagnostics are retained for review.
fn copy_ledger(source: &Path, output: &Path) -> Result<usize, String> {
    let dir = source.join("ledger");
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Err("ledger root must contain a real ledger directory".into());
    }
    fs::create_dir_all(output.join("ledger")).map_err(|e| e.to_string())?;
    let mut copied = 0;
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".seg") else {
            continue;
        };
        if stem.len() != 20 || !stem.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid ledger segment name: {name}"));
        }
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("ledger segment is not a regular file: {name}"));
        }
        fs::copy(entry.path(), output.join("ledger").join(name)).map_err(|e| e.to_string())?;
        copied += 1;
    }
    if copied == 0 {
        return Err("ledger contains no segments".into());
    }
    Ok(copied)
}

fn passed(observation: &Value) -> bool {
    observation.get("passed").and_then(Value::as_bool) == Some(true)
}

fn same_output(left: &Value, right: &Value) -> bool {
    passed(left) && passed(right) && left.get("stdout") == right.get("stdout")
}

fn provenance_ok(revision: &Value, after: &Value, status: &Value, documents: &[Value]) -> bool {
    same_output(revision, after)
        && passed(status)
        && documents.iter().all(
            |doc| match (doc.get("git_blob"), doc.get("source_blob_after")) {
                (Some(copy), Some(source)) => same_output(copy, source),
                _ => false,
            },
        )
}

fn segment_hashes(root: &Path, snapshot: &Path) -> Result<Vec<Value>, String> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(snapshot.join("ledger")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|ext| ext == "seg") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            json!({"file": path.file_name().map(|n| n.to_string_lossy()),
            "git_blob": observe(root, "git", &["hash-object", "--", &path.to_string_lossy()])})
        })
        .collect())
}

fn same_manifest(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.get("file") == b.get("file")
                && match (a.get("git_blob"), b.get("git_blob")) {
                    (Some(a), Some(b)) => same_output(a, b),
                    _ => false,
                }
        })
}

/// Collects the spec, current status/review documents, invariant matrix and optional verified ledger
/// into a new directory. The report always requires a maintainer review and never claims a phase pass.
pub fn collect(args: &[&str]) -> Result<(), String> {
    let opt = options(args)?;
    let root = repo_root()?;
    let prefix = format!("{:02}-", opt.phase);
    let mut specs = Vec::new();
    for entry in fs::read_dir(root.join("docs/spec")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&prefix))
        {
            specs.push(path);
        }
    }
    let spec = match specs.as_slice() {
        [path] => path,
        _ => return Err("phase must have exactly one specification document".into()),
    };
    let spec_bytes = fs::read(spec).map_err(|e| e.to_string())?;
    let declared = criteria(
        std::str::from_utf8(&spec_bytes).map_err(|e| e.to_string())?,
        opt.phase,
    );
    if declared.is_empty() {
        return Err("phase specification contains no exit criteria".into());
    }
    let revision = observe(&root, "git", &["rev-parse", "HEAD"]);
    if revision.get("passed").and_then(Value::as_bool) != Some(true) {
        return Err("cannot identify the source revision".into());
    }
    let status = observe(&root, "git", &["status", "--porcelain=v1"]);
    let output = root.join(&opt.output);
    // Do not mix a new report with stale evidence from a previous invocation.
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::create_dir(&output)
        .map_err(|e| format!("cannot create new package {}: {e}", output.display()))?;
    let mut documents = Vec::new();
    let mut paths = vec![spec.clone(), root.join("docs/spec/direction.md")];
    for name in [
        format!("phase-{}.md", opt.phase),
        format!("phase-{}-review.md", opt.phase),
    ] {
        let path = root.join("docs/gates").join(name);
        if path.is_file() {
            paths.push(path);
        }
    }
    for path in paths {
        let relative = path.strip_prefix(&root).map_err(|e| e.to_string())?;
        let target = output.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = if &path == spec {
            spec_bytes.clone()
        } else {
            fs::read(&path).map_err(|e| e.to_string())?
        };
        fs::write(&target, bytes).map_err(|e| e.to_string())?;
        let name = relative.to_string_lossy().into_owned();
        documents.push(
            json!({"path": name, "git_blob": observe(&root, "git", &["hash-object", "--", &target.to_string_lossy()])}),
        );
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let traceability = observe(&root, &exe.to_string_lossy(), &["traceability"]);
    let ledger = if let Some(source) = opt.ledger {
        let source = root.join(source);
        let snapshot = output.join("ledger-snapshot");
        copy_ledger(&source, &snapshot)?;
        let raw_segments = segment_hashes(&root, &snapshot)?;
        // Recovery may repair a torn tail. Keep the original evidence untouched, and retain both
        // the raw diagnosis and the replayed copy instead of silently changing the collected bytes.
        let validation = output.join("ledger-validation");
        copy_ledger(&snapshot, &validation)?;
        let dir = snapshot.to_string_lossy();
        let doctor = observe(
            &root,
            "cargo",
            &[
                "run",
                "-q",
                "-p",
                "lattice-cli",
                "--",
                "doctor",
                "--dir",
                &dir,
                "--wal",
                "ledger",
            ],
        );
        let dir = validation.to_string_lossy();
        let check = observe(
            &root,
            "cargo",
            &[
                "run",
                "-q",
                "-p",
                "lattice-cli",
                "--",
                "ledger",
                "check",
                "--dir",
                &dir,
            ],
        );
        let replayed_segments = segment_hashes(&root, &validation)?;
        let raw_after = segment_hashes(&root, &snapshot)?;
        let raw_ok = same_manifest(&raw_segments, &raw_after)
            && replayed_segments
                .iter()
                .all(|s| s.get("git_blob").is_some_and(passed));
        let changed = !same_manifest(&raw_segments, &replayed_segments);
        json!({"status": "collected", "check": check, "doctor": doctor,
            "source_root": source, "raw_segments": raw_segments, "replayed_segments": replayed_segments,
            "verification_copy_changed": changed, "raw_hashes_passed": raw_ok})
    } else {
        json!({"status": "not_provided"})
    };
    for doc in &mut documents {
        let path = doc
            .get("path")
            .and_then(Value::as_str)
            .ok_or("missing document path")?;
        doc["source_blob_after"] = observe(&root, "git", &["hash-object", "--", path]);
    }
    let revision_after = observe(&root, "git", &["rev-parse", "HEAD"]);
    let status_after = observe(&root, "git", &["status", "--porcelain=v1"]);
    // Only copied documents have content hashes. Status equality cannot prove
    // source freshness when an already dirty file changes during collection.
    let provenance_passed = provenance_ok(&revision, &revision_after, &status, &documents)
        && same_output(&status, &status_after);
    let checks_ok = provenance_passed
        && passed(&traceability)
        && (ledger.get("status") == Some(&json!("not_provided"))
            || (ledger.get("raw_hashes_passed") == Some(&json!(true))
                && ledger.pointer("/check/passed") == Some(&json!(true))
                && ledger.pointer("/doctor/passed") == Some(&json!(true))));
    let report = json!({
        "schema_version": 1, "phase": opt.phase, "review_status": "pending",
        "criteria": declared, "revision": revision, "revision_after": revision_after,
        "provenance_passed": provenance_passed,
        "provenance_scope": "revision and copied documents; source-code freshness unverified",
        "source_code_freshness": "unverified",
        "working_tree": status, "working_tree_after": status_after,
        "documents": documents, "traceability": traceability, "ledger": ledger,
    });
    fs::write(
        output.join("evidence.json"),
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "evidence package: {} (review pending; criteria are not signed)",
        output.display()
    );
    if checks_ok {
        Ok(())
    } else {
        Err("collected evidence contains a failed check; inspect evidence.json".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criteria_require_exact_phase_ids_and_never_infer_completion() {
        let rows = criteria(
            "| E1-1 | determinism | report |\n| E11-1 | other phase | nope |\n| E1-2 | all Met in prose | ledger |\n| E1-x | invalid | x |",
            1,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.first().and_then(|v| v.get("review_status")),
            Some(&json!("unverified"))
        );
        assert_eq!(
            rows.get(1).and_then(|v| v.get("required_evidence")),
            Some(&json!("ledger"))
        );
    }

    #[test]
    fn arguments_reject_missing_repeated_and_unknown_options() {
        for args in [
            vec![],
            vec!["--phase", "13"],
            vec!["--phase", "-1"],
            vec!["--phase", "1", "--phase", "2"],
            vec!["--phase", "1", "--ledger"],
            vec!["--phase", "1", "--sign", "yes"],
        ] {
            assert!(options(&args).is_err());
        }
        assert!(options(&["--phase", "12", "--output", "target/review"]).is_ok());
    }

    #[test]
    fn failed_or_changed_provenance_cannot_report_success() {
        let good = json!({"passed": true, "stdout": "same"});
        let failed = json!({"passed": false, "error": "missing git"});
        let changed = json!({"passed": true, "stdout": "changed"});
        let docs = vec![json!({"git_blob": good, "source_blob_after": good})];
        assert!(provenance_ok(&good, &good, &good, &docs));
        assert!(!provenance_ok(&good, &changed, &good, &docs));
        assert!(!provenance_ok(&good, &good, &failed, &docs));
        assert!(!provenance_ok(
            &good,
            &good,
            &good,
            &[json!({"git_blob": failed, "source_blob_after": failed})]
        ));
        assert!(!provenance_ok(
            &good,
            &good,
            &good,
            &[json!({"git_blob": good, "source_blob_after": changed})]
        ));
    }

    #[test]
    fn ledger_snapshot_excludes_locks_and_unrelated_files_and_preserves_source() {
        let base = std::env::temp_dir().join(format!("lattice-gate-{}", std::process::id()));
        let source = base.join("source");
        let target = base.join("snapshot");
        fs::create_dir_all(source.join("ledger")).unwrap();
        let name = "00000000000000000001.seg";
        fs::write(source.join("ledger").join(name), b"exact bytes").unwrap();
        fs::write(source.join("ledger/LOCK"), b"held").unwrap();
        fs::write(source.join("ledger/unrelated"), b"private").unwrap();
        assert_eq!(copy_ledger(&source, &target), Ok(1));
        assert_eq!(
            fs::read(target.join("ledger").join(name)).unwrap(),
            b"exact bytes"
        );
        assert!(!target.join("ledger/LOCK").exists());
        assert!(!target.join("ledger/unrelated").exists());
        assert_eq!(fs::read(source.join("ledger/LOCK")).unwrap(), b"held");
        let replay = base.join("replay");
        copy_ledger(&target, &replay).unwrap();
        fs::write(replay.join("ledger").join(name), b"repaired").unwrap();
        assert_eq!(
            fs::read(target.join("ledger").join(name)).unwrap(),
            b"exact bytes"
        );
        assert_eq!(
            fs::read(source.join("ledger").join(name)).unwrap(),
            b"exact bytes"
        );
        fs::remove_dir_all(base).unwrap();
    }
}
