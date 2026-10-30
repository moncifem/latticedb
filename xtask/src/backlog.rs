//! Backlog bootstrap (dogfood until Lattice hosts itself) and ADR and RFC checks.

use crate::checks::{files_with_ext, read};
use crate::repo_root;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A requirement row parsed from the spec tables.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    pub verified: String,
}

/// Parses `| P1-07 | requirement text | T, S |` rows.
pub fn parse_requirements(markdown: &str) -> Vec<Requirement> {
    let mut out = Vec::new();
    for line in markdown.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 5 {
            continue;
        }
        let id = cells[1];
        if is_requirement_id(id) {
            out.push(Requirement {
                id: id.into(),
                text: cells[2].into(),
                verified: cells[3].into(),
            });
        }
    }
    out
}

pub fn is_requirement_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix('P') else {
        return false;
    };
    let mut parts = rest.split('-');
    let (a, b) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    parts.next().is_none()
        && !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_digit())
        && b.chars().all(|c| c.is_ascii_digit())
}

fn phase_of(id: &str) -> &str {
    id.trim_start_matches('P').split('-').next().unwrap_or("")
}

pub fn all_requirements() -> Result<Vec<Requirement>, String> {
    let mut files = Vec::new();
    files_with_ext(&repo_root()?.join("docs/spec"), "md", &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for f in files {
        out.extend(parse_requirements(&read(&f)?));
    }
    Ok(out)
}

pub fn render_item(r: &Requirement) -> String {
    format!(
        "---\nid: {id}\ntype: requirement\nstate: todo\nrequirement: {id}\nphase: {phase}\nassignee: \"\"\nlinks: []\n---\n\n{text}\n\nVerification: {ver}\n",
        id = r.id,
        phase = phase_of(&r.id),
        text = r.text,
        ver = r.verified
    )
}

pub fn seed() -> Result<(), String> {
    let out = repo_root()?.join("backlog");
    fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let reqs = all_requirements()?;
    let mut created = 0;
    for r in &reqs {
        let file = out.join(format!("{}.md", r.id));
        if !file.exists() {
            fs::write(&file, render_item(r)).map_err(|e| e.to_string())?;
            created += 1;
        }
    }
    println!(
        "backlog seed: {created} created, {} requirements in spec",
        reqs.len()
    );
    Ok(())
}

const STATES: [&str; 5] = ["todo", "doing", "review", "done", "dropped"];
const FRONT_KEYS: [&str; 7] = [
    "id",
    "type",
    "state",
    "requirement",
    "phase",
    "assignee",
    "links",
];

/// Parses simple `key: value` front matter. Returns the map, or the problems found.
pub fn parse_front_matter(text: &str) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Err(vec!["missing front matter".into()]);
    }
    let mut map = BTreeMap::new();
    let mut problems = Vec::new();
    let mut closed = false;
    for l in lines.by_ref() {
        if l == "---" {
            closed = true;
            break;
        }
        match l.split_once(':') {
            Some((k, v)) => {
                if map
                    .insert(k.trim().to_string(), v.trim().to_string())
                    .is_some()
                {
                    problems.push(format!("duplicate key `{}`", k.trim()));
                }
            }
            None => problems.push(format!("malformed line `{l}`")),
        }
    }
    if !closed {
        problems.push("front matter is not closed with `---`".into());
    }
    if problems.is_empty() {
        Ok(map)
    } else {
        Err(problems)
    }
}

/// Problems with one backlog item, given its file stem and the set of valid requirement ids.
pub fn item_problems(stem: &str, text: &str, valid: &[String]) -> Vec<String> {
    let map = match parse_front_matter(text) {
        Ok(m) => m,
        Err(p) => return p,
    };
    let mut bad = Vec::new();
    for k in FRONT_KEYS {
        if !map.contains_key(k) {
            bad.push(format!("missing key `{k}`"));
        }
    }
    if map.get("id").is_some_and(|id| id != stem) {
        bad.push(format!(
            "id `{}` does not match file name `{stem}`",
            map["id"]
        ));
    }
    if let Some(s) = map.get("state")
        && !STATES.contains(&s.as_str())
    {
        bad.push(format!("state `{s}` is not one of {STATES:?}"));
    }
    if let Some(r) = map.get("requirement")
        && !r.is_empty()
        && !valid.contains(r)
    {
        bad.push(format!("requirement `{r}` does not exist in the spec"));
    }
    bad
}

fn backlog_files() -> Result<Vec<PathBuf>, String> {
    let dir = repo_root()?.join("backlog");
    let entries = fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    let mut v: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "md")
                && p.file_name().is_some_and(|n| n != "README.md")
        })
        .collect();
    v.sort();
    Ok(v)
}

pub fn item_states() -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for p in backlog_files()? {
        if let Ok(map) = parse_front_matter(&read(&p)?) {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            out.insert(stem, map.get("state").cloned().unwrap_or_default());
        }
    }
    Ok(out)
}

pub fn lint() -> Result<(), String> {
    let valid: Vec<String> = all_requirements()?.into_iter().map(|r| r.id).collect();
    let mut bad = Vec::new();
    let files = backlog_files()?;
    for p in &files {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        for problem in item_problems(stem, &read(p)?, &valid) {
            bad.push(format!("{}: {problem}", p.display()));
        }
    }
    if bad.is_empty() {
        println!("backlog lint: ok ({} items)", files.len());
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// ADR and RFC

/// `NNNN-title.md` with four ASCII digits, a dash and a lowercase ASCII slug.
pub fn is_adr_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() > 5
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn md_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter(|p| p.file_name().is_some_and(|n| n != "README.md"))
        .collect();
    v.sort();
    Ok(v)
}

/// Text of a markdown section: from the heading to the next `## ` heading.
fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let start = text.find(heading)? + heading.len();
    let rest = &text[start..];
    Some(rest.split("\n## ").next().unwrap_or(rest))
}

pub fn adr_problems(name: &str, text: &str) -> Vec<String> {
    let mut bad = Vec::new();
    if !is_adr_file_name(name) {
        bad.push(format!("{name}: file name must be NNNN-lowercase-title.md"));
    }
    if !text.starts_with("# ") {
        bad.push(format!("{name}: must start with a title"));
    }
    let status_ok = text.lines().any(|l| {
        matches!(
            l.trim(),
            "Status: proposed" | "Status: accepted" | "Status: superseded"
        )
    });
    if !status_ok {
        bad.push(format!(
            "{name}: needs `Status: proposed|accepted|superseded`"
        ));
    }
    for heading in ["## Context", "## Decision", "## Consequences"] {
        match section(text, heading) {
            None => bad.push(format!("{name}: missing section `{heading}`")),
            Some(body) if body.trim().is_empty() => {
                bad.push(format!("{name}: section `{heading}` is empty"))
            }
            _ => {}
        }
    }
    bad
}

pub fn render_adr_index(entries: &[(String, String, String)]) -> String {
    let mut out =
        String::from("# Architecture decision records\n\n| Id | Title | Status |\n|---|---|---|\n");
    for (name, title, status) in entries {
        out.push_str(&format!(
            "| [{}]({name}) | {title} | {status} |\n",
            &name[..4]
        ));
    }
    out
}

fn adr_entries() -> Result<Vec<(String, String, String)>, String> {
    let mut out = Vec::new();
    for p in md_files(&repo_root()?.join("docs/adr"))? {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        if !is_adr_file_name(&name) {
            continue;
        }
        let text = read(&p)?;
        let title = text
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("# ")
            .to_string();
        let status = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("Status: "))
            .unwrap_or("?")
            .to_string();
        out.push((name, title, status));
    }
    Ok(out)
}

/// Messages for numbers (the first four characters of a file name) used by more than one file.
/// Two branches can each pick the next free number; this catches it once they meet.
pub fn duplicate_numbers(kind: &str, names: &[&str]) -> Vec<String> {
    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in names {
        if let Some(number) = name
            .get(..4)
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            seen.entry(number).or_default().push(name);
        }
    }
    seen.into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(number, files)| format!("{kind} number {number} is used by {}", files.join(", ")))
        .collect()
}

pub fn adr_lint() -> Result<(), String> {
    let dir = repo_root()?.join("docs/adr");
    let mut bad = Vec::new();
    let files = md_files(&dir)?;
    for p in &files {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        bad.extend(adr_problems(name, &read(p)?));
    }
    let names: Vec<&str> = files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect();
    bad.extend(duplicate_numbers("ADR", &names));
    let expected = render_adr_index(&adr_entries()?);
    if fs::read_to_string(dir.join("README.md")).ok().as_deref() != Some(expected.as_str()) {
        bad.push("docs/adr/README.md is stale: run `cargo xtask adr index`".into());
    }
    if bad.is_empty() {
        println!("adr lint: ok ({} records)", files.len());
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

pub fn adr_index() -> Result<(), String> {
    let out = render_adr_index(&adr_entries()?);
    fs::write(repo_root()?.join("docs/adr/README.md"), out).map_err(|e| e.to_string())?;
    println!("adr index written");
    Ok(())
}

/// RFCs need a title, a status line, a comment-period line and the required sections.
pub fn rfc_problems(name: &str, text: &str) -> Vec<String> {
    let mut bad = Vec::new();
    if !text.starts_with("# ") {
        bad.push(format!("{name}: must start with a title"));
    }
    for line in ["Status:", "Comment period ends:", "Decision ADR:"] {
        if !text.lines().any(|l| l.trim().starts_with(line)) {
            bad.push(format!("{name}: needs a `{line}` line"));
        }
    }
    for heading in ["## Motivation", "## Proposal", "## Alternatives"] {
        if section(text, heading).is_none_or(|b| b.trim().is_empty()) {
            bad.push(format!("{name}: missing or empty `{heading}`"));
        }
    }
    bad
}

pub fn rfc_lint() -> Result<(), String> {
    let dir = repo_root()?.join("docs/rfc");
    let mut bad = Vec::new();
    let files: Vec<PathBuf> = md_files(&dir)?
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n != "TEMPLATE.md"))
        .collect();
    for p in &files {
        bad.extend(rfc_problems(
            p.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            &read(p)?,
        ));
    }
    let names: Vec<&str> = files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect();
    bad.extend(duplicate_numbers("RFC", &names));
    if bad.is_empty() {
        println!("rfc lint: ok ({} rfcs)", files.len());
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requirement_rows_only() {
        let md = "| ID | Requirement | Verified |\n|---|---|---|\n| P1-07 | Do the thing | T, S |\n| E1-1 | criterion | evidence |\n| P1-07-9 | x | y |\n";
        let r = parse_requirements(md);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, "P1-07");
        assert!(
            is_requirement_id("P12-03") && !is_requirement_id("P-1") && !is_requirement_id("PX-1")
        );
    }

    #[test]
    fn rendered_item_is_valid_for_its_own_requirement() {
        let r = Requirement {
            id: "P0-01".into(),
            text: "x".into(),
            verified: "I".into(),
        };
        let item = render_item(&r);
        assert!(item_problems("P0-01", &item, &["P0-01".into()]).is_empty());
        assert_eq!(phase_of("P12-03"), "12");
    }

    #[test]
    fn backlog_lint_is_strict() {
        let valid = vec!["P0-01".to_string()];
        let good = "---\nid: P0-01\ntype: requirement\nstate: todo\nrequirement: P0-01\nphase: 0\nassignee: \"\"\nlinks: []\n---\nbody";
        assert!(item_problems("P0-01", good, &valid).is_empty());
        assert!(
            !item_problems("P0-02", good, &valid).is_empty(),
            "id must match file name"
        );
        assert!(!item_problems("P0-01", &good.replace("todo", "wip"), &valid).is_empty());
        assert!(
            !item_problems(
                "P0-01",
                &good.replace("requirement: P0-01", "requirement: P9-99"),
                &valid
            )
            .is_empty()
        );
        assert!(!item_problems("P0-01", "---\nid: x\nid: y\n---\n", &valid).is_empty());
        assert!(
            !item_problems("P0-01", "---\nid: P0-01\n", &valid).is_empty(),
            "unclosed"
        );
        assert!(!item_problems("P0-01", "no front matter", &valid).is_empty());
    }

    #[test]
    fn adr_checks_names_sections_and_unicode_safely() {
        let good =
            "# t\nStatus: accepted\n## Context\nwhy\n## Decision\nwhat\n## Consequences\nso\n";
        assert!(adr_problems("0001-x.md", good).is_empty());
        assert!(
            !adr_problems("0001-x.md", &good.replace("why", "")).is_empty(),
            "empty section"
        );
        assert!(
            !adr_problems("éaé.md", good).is_empty(),
            "unicode name must not panic"
        );
        assert!(
            !is_adr_file_name("1-x.md")
                && !is_adr_file_name("0001-X.md")
                && is_adr_file_name("0001-x.md")
        );
    }

    #[test]
    fn two_files_with_one_number_are_reported() {
        assert!(duplicate_numbers("ADR", &["0001-a.md", "0002-b.md"]).is_empty());
        let dup = duplicate_numbers("ADR", &["0025-a.md", "0025-b.md", "0026-c.md"]);
        assert_eq!(dup, ["ADR number 0025 is used by 0025-a.md, 0025-b.md"]);
    }

    #[test]
    fn rfc_checks_required_fields() {
        let good = "# r\nStatus: draft\nComment period ends: 2026-11-01\nDecision ADR: none yet\n## Motivation\nm\n## Proposal\np\n## Alternatives\na\n";
        assert!(rfc_problems("x.md", good).is_empty());
        assert!(rfc_problems("x.md", "# r\n").len() >= 4);
    }

    #[test]
    fn index_is_deterministic() {
        let e = vec![(
            "0001-a.md".to_string(),
            "A".to_string(),
            "accepted".to_string(),
        )];
        assert_eq!(render_adr_index(&e), render_adr_index(&e));
        assert!(render_adr_index(&e).contains("[0001](0001-a.md)"));
    }
}
