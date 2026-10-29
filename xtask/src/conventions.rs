//! Convention lints and the commit message check.

use crate::checks::{crate_dir, files_with_ext, parse_layers, read};
use crate::repo_root;

const CHANGE_TYPES: [&str; 10] = [
    "feat", "fix", "docs", "chore", "refactor", "test", "perf", "ci", "build", "revert",
];
const EXTRA_SCOPES: [&str; 14] = [
    "web", "sidecars", "packs", "formats", "tla", "evals", "benches", "fuzz", "backlog", "docs",
    "tools", "xtask", "deps", "spec",
];

/// Problems with a Conventional Commits first line. `scopes` are the allowed scope names.
pub fn commit_problems(message: &str, scopes: &[String]) -> Vec<String> {
    let first = message.lines().next().unwrap_or("");
    let Some((head, subject)) = first.split_once(": ") else {
        return vec![format!("`{first}`: expected `type(scope): subject`")];
    };
    let mut bad = Vec::new();
    let head = head.trim_end_matches('!');
    let (kind, scope) = match head.split_once('(') {
        Some((k, rest)) => (k, rest.strip_suffix(')')),
        None => (head, None),
    };
    if !CHANGE_TYPES.contains(&kind) {
        bad.push(format!(
            "unknown type `{kind}`, use one of {CHANGE_TYPES:?}"
        ));
    }
    if let Some(s) = scope {
        if !scopes.iter().any(|x| x == s) {
            bad.push(format!(
                "unknown scope `{s}`: use a crate or directory name"
            ));
        }
    } else if head.contains('(') {
        bad.push("malformed scope".into());
    }
    if subject.trim().is_empty() {
        bad.push("empty subject".into());
    }
    bad
}

fn allowed_scopes() -> Result<Vec<String>, String> {
    let layers = parse_layers(&read(&repo_root()?.join("layers.toml"))?)?;
    // Both `lattice-core` and the shorter `core` are accepted as a scope.
    let mut v: Vec<String> = layers.crates.keys().cloned().collect();
    v.extend(
        layers
            .crates
            .keys()
            .filter_map(|k| k.strip_prefix("lattice-"))
            .map(String::from),
    );
    v.extend(EXTRA_SCOPES.iter().map(|s| (*s).to_string()));
    Ok(v)
}

pub fn commit_lint(file: &str) -> Result<(), String> {
    let msg = read(std::path::Path::new(file))?;
    let bad = commit_problems(&msg, &allowed_scopes()?);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("commit message: {}", bad.join("; ")))
    }
}

/// Lines whose comment part has a TODO or FIXME that is not followed by `(P<n>-<nn>)`.
pub fn untracked_todos(src: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let Some((_, comment)) = line.split_once("//") else {
            continue;
        };
        for tag in ["TODO", "FIXME"] {
            if let Some(pos) = comment.find(tag) {
                let rest = &comment[pos + tag.len()..];
                let tracked = rest
                    .strip_prefix('(')
                    .and_then(|r| r.split(')').next())
                    .is_some_and(crate::backlog::is_requirement_id);
                if !tracked {
                    out.push(i + 1);
                }
            }
        }
    }
    out
}

pub fn lint_conventions() -> Result<(), String> {
    let root = repo_root()?;
    let layers = parse_layers(&read(&root.join("layers.toml"))?)?;
    let mut bad = Vec::new();
    for name in layers.crates.keys() {
        let ok_name = name.starts_with("lattice-") || name == "latticed" || name == "xtask";
        if !ok_name {
            bad.push(format!(
                "crate `{name}` must be named lattice-<name> or latticed"
            ));
        }
        if !crate_dir(&root, name).join("README.md").exists() {
            bad.push(format!(
                "crate `{name}` has no README.md stating its responsibility"
            ));
        }
    }
    for dir in [
        "web", "sidecars", "packs", "formats", "tla", "evals", "benches", "fuzz", "backlog",
        "tools",
    ] {
        if !root.join(dir).join("README.md").exists() {
            bad.push(format!("{dir}/README.md is missing"));
        }
    }
    let mut files = Vec::new();
    files_with_ext(&root.join("crates"), "rs", &mut files)?;
    for f in files {
        for line in untracked_todos(&read(&f)?) {
            bad.push(format!(
                "{}:{line}: TODO needs a requirement id, e.g. TODO(P1-07)",
                f.display()
            ));
        }
    }
    if bad.is_empty() {
        println!("lint-conventions: ok");
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<String> {
        vec!["lattice-core".into(), "docs".into()]
    }

    #[test]
    fn conventional_commits_are_checked() {
        assert!(commit_problems("feat(lattice-core): add commit pipeline", &scopes()).is_empty());
        assert!(commit_problems("fix: quick fix", &scopes()).is_empty());
        assert!(commit_problems("feat(lattice-core)!: breaking", &scopes()).is_empty());
        assert!(!commit_problems("added stuff", &scopes()).is_empty());
        assert!(!commit_problems("wip(lattice-core): x", &scopes()).is_empty());
        assert!(!commit_problems("feat(nope): x", &scopes()).is_empty());
        assert!(commit_problems("feat(core): short crate scope", &["core".to_string()]).is_empty());
        assert!(!commit_problems("feat(docs): ", &scopes()).is_empty());
    }

    #[test]
    fn todos_need_a_requirement_id() {
        assert_eq!(
            untracked_todos("let a = 1; // TODO fix\n// TODO(P1-07): later\n// FIXME(x)"),
            vec![1, 3]
        );
        assert!(untracked_todos("// nothing here").is_empty());
    }
}
