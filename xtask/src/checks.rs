//! Structural checks: crate layers, unsafe discipline, banned-call allowlist, format corpus,
//! tool config formats, links, test quarantine, and requirement and invariant traceability.

use crate::lex;
use crate::repo_root;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Collects files with the given extension below `dir`, skipping build output.
pub fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
    };
    for entry in entries {
        let p = entry.map_err(|e| e.to_string())?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(name, "target" | ".git" | "node_modules") {
            continue;
        }
        if p.is_dir() {
            files_with_ext(&p, ext, out)?;
        } else if p.extension().is_some_and(|e| e == ext) {
            out.push(p);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// layers

/// Parsed `layers.toml`: crate name -> (layer, allowed normal dependencies).
pub struct Layers {
    pub crates: BTreeMap<String, (i64, Vec<String>)>,
    pub dev_allow: Vec<String>,
}

fn string_list(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_layers(text: &str) -> Result<Layers, String> {
    let v: toml::Value = toml::from_str(text).map_err(|e| format!("layers.toml: {e}"))?;
    let dev_allow = string_list(v.get("dev_allow"));
    let table = v
        .get("crate")
        .and_then(|c| c.as_table())
        .ok_or("layers.toml: no [crate.*]")?;
    let mut crates = BTreeMap::new();
    for (name, entry) in table {
        let layer = entry
            .get("layer")
            .and_then(|l| l.as_integer())
            .ok_or(format!("{name}: no layer"))?;
        crates.insert(name.clone(), (layer, string_list(entry.get("allow"))));
    }
    Ok(Layers { crates, dev_allow })
}

/// Test-support crates may only be dev-dependencies of crates at or above this layer.
const DEV_ALLOW_MIN_LAYER: i64 = 2;

/// Returns the list of violations for the given metadata JSON.
pub fn layer_violations(layers: &Layers, metadata: &serde_json::Value) -> Vec<String> {
    let mut bad = Vec::new();
    for (name, (layer, allow)) in &layers.crates {
        for a in allow {
            match layers.crates.get(a) {
                None => bad.push(format!("{name}: allows unknown crate {a}")),
                Some((al, _)) if al > layer => bad.push(format!(
                    "{name} (L{layer}) allows higher layer crate {a} (L{al})"
                )),
                _ => {}
            }
        }
    }
    let empty = Vec::new();
    let packages = metadata
        .get("packages")
        .and_then(|p| p.as_array())
        .unwrap_or(&empty);
    for pkg in packages {
        let Some(name) = pkg.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some((layer, allow)) = layers.crates.get(name) else {
            bad.push(format!(
                "{name}: workspace package missing from layers.toml"
            ));
            continue;
        };
        for dep in pkg
            .get("dependencies")
            .and_then(|d| d.as_array())
            .unwrap_or(&empty)
        {
            let Some(dn) = dep.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if !layers.crates.contains_key(dn) {
                continue;
            }
            let kind = dep.get("kind").and_then(|k| k.as_str()).unwrap_or("normal");
            let listed = allow.iter().any(|a| a == dn);
            let dev_ok = kind == "dev"
                && *layer >= DEV_ALLOW_MIN_LAYER
                && layers.dev_allow.iter().any(|a| a == dn);
            if !(listed || dev_ok) {
                bad.push(format!(
                    "{name} has a {kind} dependency on {dn}, which layers.toml does not allow"
                ));
            }
        }
    }
    // Every declared crate must exist, so layers.toml cannot rot.
    let present: Vec<&str> = packages
        .iter()
        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
        .collect();
    for name in layers.crates.keys() {
        if !present.contains(&name.as_str()) {
            bad.push(format!(
                "{name}: declared in layers.toml but not a workspace package"
            ));
        }
    }
    bad
}

pub fn check_layers() -> Result<(), String> {
    let root = repo_root()?;
    let layers = parse_layers(&read(&root.join("layers.toml"))?)?;
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(&root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".into());
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let bad = layer_violations(&layers, &meta);
    if bad.is_empty() {
        println!("check-layers: ok ({} crates)", layers.crates.len());
        Ok(())
    } else {
        Err(format!("layer violations:\n  {}", bad.join("\n  ")))
    }
}

pub fn crate_dir(root: &Path, name: &str) -> PathBuf {
    if name == "xtask" {
        root.join("xtask")
    } else {
        root.join("crates").join(name)
    }
}

// ---------------------------------------------------------------------------------------------
// unsafe discipline

/// Whether the manifest forbids unsafe code through the workspace table or its own lint table.
pub fn manifest_forbids_unsafe(manifest: &str) -> bool {
    let Ok(v) = toml::from_str::<toml::Value>(manifest) else {
        return false;
    };
    let lints = v.get("lints");
    let inherits = lints
        .and_then(|l| l.get("workspace"))
        .and_then(|w| w.as_bool())
        == Some(true);
    let own = lints
        .and_then(|l| l.get("rust"))
        .and_then(|r| r.get("unsafe_code"))
        .and_then(|u| u.as_str())
        == Some("forbid");
    inherits || own
}

/// Counts `unsafe` tokens (real code only).
pub fn unsafe_count(src: &str) -> usize {
    lex::count_word(&lex::code_only(src), "unsafe")
}

/// Every crate except `lattice-sys` must forbid unsafe code (manifest) and contain no `unsafe`
/// token in any Rust file (src, tests, benches, examples, build scripts).
pub fn check_unsafe() -> Result<(), String> {
    let root = repo_root()?;
    let layers = parse_layers(&read(&root.join("layers.toml"))?)?;
    let mut bad = Vec::new();
    for name in layers.crates.keys() {
        let dir = crate_dir(&root, name);
        let mut files = Vec::new();
        files_with_ext(&dir, "rs", &mut files)?;
        let mut total = 0;
        for f in &files {
            let n = unsafe_count(&read(f)?);
            total += n;
            if n > 0 && name != "lattice-sys" {
                bad.push(format!("{}: contains {n} `unsafe` token(s)", f.display()));
            }
        }
        if name == "lattice-sys" {
            println!("check-unsafe: lattice-sys contains {total} unsafe token(s) (inventory)");
        } else if !manifest_forbids_unsafe(&read(&dir.join("Cargo.toml"))?) {
            bad.push(format!(
                "{name}: manifest neither inherits workspace lints nor forbids unsafe_code"
            ));
        }
    }
    if bad.is_empty() {
        println!("check-unsafe: ok");
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// banned-call allowlist

/// Any mention of the determinism lints in code or manifests of a non-allowlisted crate is a
/// violation, whatever the attribute form (`allow`, `expect`, `cfg_attr`, multi-line, tables).
pub fn banned_lint_mentions(rust_src: &str) -> bool {
    lex::code_only(rust_src).contains("disallowed_")
}

pub fn manifest_mentions_banned(manifest: &str) -> bool {
    manifest
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .any(|l| l.contains("disallowed_"))
}

pub fn check_allow() -> Result<(), String> {
    let root = repo_root()?;
    let v: toml::Value = toml::from_str(&read(&root.join("tools/allow-banned.toml"))?)
        .map_err(|e| format!("allow-banned.toml: {e}"))?;
    let allowed = string_list(v.get("crates"));
    let layers = parse_layers(&read(&root.join("layers.toml"))?)?;
    let mut bad = Vec::new();
    for name in layers.crates.keys().filter(|n| !allowed.contains(n)) {
        let dir = crate_dir(&root, name);
        let mut files = Vec::new();
        files_with_ext(&dir, "rs", &mut files)?;
        for f in files {
            if banned_lint_mentions(&read(&f)?) {
                bad.push(format!(
                    "{}: {name} may not suppress determinism lints",
                    f.display()
                ));
            }
        }
        if manifest_mentions_banned(&read(&dir.join("Cargo.toml"))?) {
            bad.push(format!("{name}: Cargo.toml mentions determinism lints"));
        }
    }
    let cfg = root.join(".cargo/config.toml");
    if let Ok(text) = fs::read_to_string(&cfg)
        && text.contains("disallowed_")
    {
        bad.push(format!(
            "{}: global lint flags may not mention determinism lints",
            cfg.display()
        ));
    }
    if bad.is_empty() {
        println!("check-allow: ok");
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// format corpus and tool config formats

/// Every `*.golden` file under formats/corpus (recursively) needs a `*.golden.toml` with string
/// `format` and positive integer `version`.
pub fn check_format_corpus() -> Result<(), String> {
    let dir = repo_root()?.join("formats/corpus");
    if !dir.is_dir() {
        return Err(format!(
            "{} is missing (keep a .gitkeep in it)",
            dir.display()
        ));
    }
    let mut files = Vec::new();
    files_with_ext(&dir, "golden", &mut files)?;
    for p in &files {
        let meta = PathBuf::from(format!("{}.toml", p.display()));
        let text = read(&meta).map_err(|_| format!("{}: missing metadata file", p.display()))?;
        let v: toml::Value =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", meta.display()))?;
        let format_ok = v.get("format").is_some_and(toml::Value::is_str);
        let version_ok = v
            .get("version")
            .and_then(|x| x.as_integer())
            .is_some_and(|n| n > 0);
        if !format_ok || !version_ok {
            return Err(format!(
                "{}: needs string `format` and positive integer `version`",
                meta.display()
            ));
        }
    }
    println!("check-format-corpus: ok ({} golden files)", files.len());
    Ok(())
}

pub fn profile_problems(name: &str, text: &str) -> Vec<String> {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return vec![format!("{name}: invalid TOML")];
    };
    let mut bad = Vec::new();
    for k in ["name", "cpu", "storage"] {
        if !v.get(k).is_some_and(toml::Value::is_str) {
            bad.push(format!("{name}: `{k}` must be a string"));
        }
    }
    if !v
        .get("memory_gb")
        .and_then(|m| m.as_integer())
        .is_some_and(|m| m > 0)
    {
        bad.push(format!("{name}: `memory_gb` must be a positive integer"));
    }
    bad
}

pub fn thresholds_problems(name: &str, text: &str) -> Vec<String> {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return vec![format!("{name}: invalid TOML")];
    };
    let Some(metrics) = v.get("metric").and_then(|m| m.as_array()) else {
        return vec![format!("{name}: needs at least one [[metric]]")];
    };
    let num = |x: &toml::Value, k: &str| -> Option<f64> {
        x.get(k)
            .and_then(|n| n.as_float().or_else(|| n.as_integer().map(|i| i as f64)))
    };
    let mut bad = Vec::new();
    for (i, m) in metrics.iter().enumerate() {
        if !m.get("name").is_some_and(toml::Value::is_str) {
            bad.push(format!("{name}: metric {i}: `name` must be a string"));
        }
        if num(m, "lower_bound").is_none() {
            bad.push(format!(
                "{name}: metric {i}: `lower_bound` must be a number"
            ));
        }
        if !num(m, "confidence").is_some_and(|c| c > 0.0 && c < 1.0) {
            bad.push(format!(
                "{name}: metric {i}: `confidence` must be in (0, 1)"
            ));
        }
        if !num(m, "tolerance").is_some_and(|t| t >= 0.0) {
            bad.push(format!("{name}: metric {i}: `tolerance` must be >= 0"));
        }
    }
    bad
}

pub fn check_config_formats() -> Result<(), String> {
    let root = repo_root()?;
    let mut bad = Vec::new();
    let mut n = 0;
    let mut files = Vec::new();
    files_with_ext(&root.join("benches/profiles"), "toml", &mut files)?;
    for f in &files {
        n += 1;
        bad.extend(profile_problems(&f.display().to_string(), &read(f)?));
    }
    let mut files = Vec::new();
    files_with_ext(&root.join("evals"), "toml", &mut files)?;
    for f in files
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| n == "thresholds.toml"))
    {
        n += 1;
        bad.extend(thresholds_problems(&f.display().to_string(), &read(f)?));
    }
    if bad.is_empty() {
        println!("check-config-formats: ok ({n} files)");
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// markdown links

/// Relative link targets (without anchors) found in markdown text.
pub fn relative_links(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(i) = rest.find("](") {
        let after = &rest[i + 2..];
        if let Some(end) = after.find(')') {
            let target = after[..end].trim();
            let path = target.split('#').next().unwrap_or("");
            let external = target.starts_with("http") || target.starts_with("mailto:");
            if !path.is_empty() && !external && !path.contains(' ') {
                out.push(path.to_string());
            }
            rest = &after[end..];
        } else {
            break;
        }
    }
    out
}

pub fn check_links() -> Result<(), String> {
    let root = repo_root()?;
    let mut files = Vec::new();
    files_with_ext(&root, "md", &mut files)?;
    let mut bad = Vec::new();
    for f in files
        .iter()
        .filter(|f| !f.starts_with(root.join("backlog")))
    {
        let text = read(f)?;
        let dir = f.parent().unwrap_or(&root);
        for l in relative_links(&text) {
            if !dir.join(&l).exists() {
                bad.push(format!(
                    "{}: broken link `{l}`",
                    f.strip_prefix(&root).unwrap_or(f).display()
                ));
            }
        }
    }
    if bad.is_empty() {
        println!("check-links: ok ({} markdown files)", files.len());
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// flaky test quarantine

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub fn quarantine_problems(text: &str, today_days: i64) -> Vec<String> {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return vec!["quarantine.toml: invalid TOML".into()];
    };
    let mut bad = Vec::new();
    for (i, t) in v
        .get("test")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .enumerate()
    {
        for k in ["name", "owner", "reason", "expires"] {
            if !t.get(k).is_some_and(toml::Value::is_str) {
                bad.push(format!("quarantine test {i}: `{k}` must be a string"));
            }
        }
        match t
            .get("expires")
            .and_then(|e| e.as_str())
            .and_then(parse_date)
        {
            None => bad.push(format!("quarantine test {i}: `expires` must be YYYY-MM-DD")),
            Some(d) if d < today_days => bad.push(format!(
                "quarantine test {i}: quarantine expired, fix or remove the test"
            )),
            _ => {}
        }
    }
    bad
}

pub fn check_quarantine() -> Result<(), String> {
    let path = repo_root()?.join("tools/quarantine.toml");
    let text = if path.exists() {
        read(&path)?
    } else {
        String::new()
    };
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs() / 86_400).unwrap_or(0))
        .unwrap_or(0);
    let bad = quarantine_problems(&text, today);
    let count = toml::from_str::<toml::Value>(&text)
        .ok()
        .and_then(|v| v.get("test").and_then(|t| t.as_array()).map(Vec::len))
        .unwrap_or(0);
    if bad.is_empty() {
        println!("check-quarantine: ok ({count} quarantined tests)");
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// traceability

/// Invariant test names: `fn inv_i<N>_...` (N in 1..=14) preceded by a test attribute.
pub fn invariant_matrix(files: &[(String, String)]) -> BTreeMap<u32, Vec<String>> {
    let mut m: BTreeMap<u32, Vec<String>> = (1..=14).map(|i| (i, Vec::new())).collect();
    for (file, text) in files {
        let code = lex::code_only(text);
        let lines: Vec<&str> = code.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let l = line.trim();
            let l = l.strip_prefix("async ").unwrap_or(l);
            let Some(rest) = l.strip_prefix("fn inv_i") else {
                continue;
            };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let Ok(n) = digits.parse::<u32>() else {
                continue;
            };
            if !(1..=14).contains(&n) {
                continue;
            }
            // Only the attribute lines directly above the function count.
            let mut is_test = false;
            for prev in lines[..idx].iter().rev() {
                let p = prev.trim();
                if p.is_empty() || p.starts_with("#[") {
                    is_test |=
                        p.contains("#[test") || p.contains("::test") || p.contains("test_case");
                } else {
                    break;
                }
            }
            if is_test {
                let name = l
                    .trim_start_matches("fn ")
                    .split('(')
                    .next()
                    .unwrap_or("")
                    .to_string();
                m.entry(n).or_default().push(format!("{file}::{name}"));
            }
        }
    }
    m
}

/// Requirement ids named in `Requirement: P<phase>-<nn>` trailers.
pub fn trailer_requirements(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| l.trim().strip_prefix("Requirement:"))
        .flat_map(|rest| {
            rest.split([',', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(String::from)
        .collect()
}

pub fn traceability() -> Result<(), String> {
    let root = repo_root()?;
    let mut paths = Vec::new();
    files_with_ext(&root.join("crates"), "rs", &mut paths)?;
    let mut files = Vec::new();
    for p in paths {
        let rel = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
        files.push((rel, read(&p)?));
    }
    let m = invariant_matrix(&files);
    println!("invariant traceability (tests named inv_i<N>_* with a test attribute):");
    for (n, tests) in &m {
        println!("  I{n:<2} {} test(s)", tests.len());
        for t in tests {
            println!("       {t}");
        }
    }
    let reqs = crate::backlog::all_requirements()?;
    let log = Command::new("git")
        .args(["log", "--format=%B"])
        .current_dir(&root)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let named: Vec<String> = trailer_requirements(&log);
    let states = crate::backlog::item_states()?;
    let finished = states.values().filter(|s| s.as_str() == "done").count();
    println!(
        "requirements: {} in spec, {} backlog items ({} done), {} referenced by commit trailers",
        reqs.len(),
        states.len(),
        finished,
        reqs.iter().filter(|r| named.contains(&r.id)).count()
    );
    let unknown: Vec<&String> = named
        .iter()
        .filter(|d| !reqs.iter().any(|r| &r.id == *d))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "commit trailers name unknown requirements: {unknown:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layers() -> Layers {
        parse_layers(
            "dev_allow = [\"kit\"]\n[crate.low]\nlayer = 0\nallow = []\n[crate.high]\nlayer = 2\nallow = [\"low\"]\n[crate.kit]\nlayer = 8\nallow = []\n",
        )
        .unwrap()
    }

    fn meta(probe: &str, deps: &[(&str, &str)]) -> serde_json::Value {
        let list: Vec<_> = deps
            .iter()
            .map(|(n, k)| {
                serde_json::json!({"name": n, "kind": if *k == "normal" { serde_json::Value::Null } else { serde_json::json!(k) }})
            })
            .collect();
        serde_json::json!({"packages": [
            {"name": "low", "dependencies": []},
            {"name": "high", "dependencies": []},
            {"name": "kit", "dependencies": []},
            {"name": probe, "dependencies": list}
        ]})
    }

    #[test]
    fn allowed_edge_passes_and_forbidden_edge_fails() {
        let mut layers = sample_layers();
        layers
            .crates
            .insert("probe".into(), (2, vec!["low".into()]));
        assert!(layer_violations(&layers, &meta("probe", &[("low", "normal")])).is_empty());
        assert_eq!(
            layer_violations(&layers, &meta("probe", &[("high", "normal")])).len(),
            1
        );
    }

    #[test]
    fn dev_allow_needs_layer_two_or_higher_and_dev_kind() {
        let mut layers = sample_layers();
        layers.crates.insert("probe".into(), (2, vec![]));
        assert!(layer_violations(&layers, &meta("probe", &[("kit", "dev")])).is_empty());
        assert_eq!(
            layer_violations(&layers, &meta("probe", &[("kit", "normal")])).len(),
            1
        );
        layers.crates.insert("probe".into(), (1, vec![]));
        assert_eq!(
            layer_violations(&layers, &meta("probe", &[("kit", "dev")])).len(),
            1
        );
    }

    #[test]
    fn higher_layer_allow_and_undeclared_or_ghost_members_are_rejected() {
        let layers = parse_layers(
            "[crate.a]\nlayer = 0\nallow = [\"b\"]\n[crate.b]\nlayer = 3\nallow = []\n",
        )
        .unwrap();
        let bad = layer_violations(&layers, &serde_json::json!({"packages": []}));
        assert!(bad.iter().any(|b| b.contains("higher layer")));
        assert!(bad.iter().any(|b| b.contains("not a workspace package")));
        let one = parse_layers("[crate.a]\nlayer = 0\nallow = []\n").unwrap();
        let bad = layer_violations(
            &one,
            &serde_json::json!({"packages": [{"name": "ghost", "dependencies": []}]}),
        );
        assert!(bad.iter().any(|b| b.contains("missing from layers.toml")));
    }

    #[test]
    fn unsafe_detection_ignores_strings_and_sees_real_code() {
        assert_eq!(unsafe_count("let s = \"unsafe\"; // unsafe\n"), 0);
        assert_eq!(unsafe_count("let u = \"https://x\"; unsafe { f() }"), 1);
        assert_eq!(unsafe_count("unsafe\nfn x() {}"), 1);
    }

    #[test]
    fn manifest_unsafe_policy() {
        assert!(manifest_forbids_unsafe("[lints]\nworkspace = true\n"));
        assert!(manifest_forbids_unsafe(
            "[lints.rust]\nunsafe_code = \"forbid\"\n"
        ));
        assert!(!manifest_forbids_unsafe("[package]\nname = \"x\"\n"));
        assert!(!manifest_forbids_unsafe(
            "[lints.rust]\nunsafe_code = \"allow\"\n"
        ));
    }

    #[test]
    fn banned_lint_suppression_forms_are_all_caught() {
        for src in [
            "#![allow(clippy::disallowed_methods)]",
            "#[expect(clippy::disallowed_methods)]\nfn f() {}",
            "#![cfg_attr(test, allow(clippy::disallowed_types))]",
            "#![allow(\n    clippy::disallowed_methods,\n)]",
        ] {
            assert!(banned_lint_mentions(src), "{src}");
        }
        assert!(!banned_lint_mentions(
            "// clippy::disallowed_methods is mentioned in a comment"
        ));
        assert!(manifest_mentions_banned(
            "[lints.clippy]\ndisallowed_methods = \"allow\"\n"
        ));
        assert!(!manifest_mentions_banned(
            "# disallowed_methods only in a comment\n"
        ));
    }

    #[test]
    fn config_formats_are_validated() {
        assert!(
            profile_problems("p", "name=\"a\"\ncpu=\"b\"\nmemory_gb=8\nstorage=\"c\"\n").is_empty()
        );
        assert!(!profile_problems("p", "name=\"a\"\nmemory_gb=-1\n").is_empty());
        let ok = "[[metric]]\nname=\"f1\"\nlower_bound=0.8\nconfidence=0.95\ntolerance=0.01\n";
        assert!(thresholds_problems("t", ok).is_empty());
        let bad = "[[metric]]\nname=\"f1\"\nlower_bound=0.8\nconfidence=1.5\ntolerance=-1\n";
        assert_eq!(thresholds_problems("t", bad).len(), 2);
        assert!(!thresholds_problems("t", "x=1").is_empty());
    }

    #[test]
    fn link_extraction_skips_external_and_anchors() {
        let l =
            relative_links("[a](x.md) [b](https://e.com) [c](#top) [d](y.md#sec) [e](mailto:a@b)");
        assert_eq!(l, vec!["x.md", "y.md"]);
    }

    #[test]
    fn quarantine_requires_owner_and_unexpired_date() {
        let today = parse_date("2026-10-28").unwrap();
        let good = "[[test]]\nname=\"t\"\nowner=\"o\"\nreason=\"r\"\nexpires=\"2026-11-01\"\n";
        assert!(quarantine_problems(good, today).is_empty());
        let old = good.replace("2026-11-01", "2026-10-01");
        assert!(!quarantine_problems(&old, today).is_empty());
        assert!(!quarantine_problems("[[test]]\nname=\"t\"\n", today).is_empty());
        assert_eq!(parse_date("1970-01-02"), Some(1));
        assert_eq!(parse_date("2026-13-01"), None);
    }

    #[test]
    fn matrix_requires_a_test_attribute_and_valid_id() {
        let src = "#[test]\nfn inv_i1_acked_commit_survives() {}\nfn inv_i6_not_a_test() {}\n#[test]\nfn inv_i99_bad_id() {}\n#[tokio::test]\nasync fn inv_i2_atomic() {}";
        let m = invariant_matrix(&[("f.rs".into(), src.into())]);
        assert_eq!(m[&1].len(), 1);
        assert_eq!(m[&6].len(), 0);
        assert_eq!(m[&2].len(), 1);
        assert!(!m.contains_key(&99));
    }

    #[test]
    fn trailers_are_extracted() {
        let t = trailer_requirements("feat: x\n\nRequirement: P1-07, P1-08\nSigned-off-by: a\n");
        assert_eq!(t, vec!["P1-07", "P1-08"]);
    }
}
