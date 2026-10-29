#!/usr/bin/env python3
"""Pull request governance: risk classification and open-PR path claims.

The pure functions (classify_risk, required_label_ok, overlap) have no I/O and are unit tested
in test_pr_governance.py. The command line wraps them for .github/workflows/pr-governance.yml.
Keep RISK_RULES in step with .github/CODEOWNERS and docs/review-policy.md.
"""

import argparse
import json
import re
import subprocess
import sys

REVIEWED_LABEL = "reviewed"
OVERLAP_LABEL = "overlap"
COMMENT_MARKER = "<!-- pr-governance-claims -->"
COMMENT_AUTHOR = "github-actions[bot]"

# Files that never count as a claim, because many PRs legitimately touch them.
EXEMPT_GLOBS = (
    "CHANGELOG.md",
    "docs/gates/**",
    "fuzz/README.md",
    "fuzz/Cargo.toml",
)

# (area, globs). `**` crosses directories, `*` does not.
RISK_RULES = (
    ("format", ("formats/**",)),
    ("kernel", ("crates/lattice-core/**",)),
    (
        "auth",
        (
            "crates/lattice-api/**/auth*",
            "crates/lattice-api/**/scram*",
        ),
    ),
    ("layers", ("layers.toml",)),
    ("workflows", (".github/**",)),
    (
        "protection",
        (
            "tools/branch-protection.json",
            "tools/pr_governance.py",
            "tools/test_pr_governance.py",
        ),
    ),
    ("rfc-adr", ("docs/rfc/**", "docs/adr/**")),
)

MAX_OPEN_PRS = 200


def _glob_regex(glob):
    out = []
    i = 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


def matches(path, globs):
    return any(_glob_regex(g).match(path) for g in globs)


def classify_risk(changed_paths):
    """Return the set of risk areas touched by the changed paths."""
    areas = set()
    for path in changed_paths:
        for area, globs in RISK_RULES:
            if matches(path, globs):
                areas.add(area)
    return areas


def required_label_ok(labels, risk):
    """A PR with no risk areas always passes; otherwise it needs the `reviewed` label."""
    return not risk or REVIEWED_LABEL in set(labels)


def overlap(my_paths, other_prs_paths, exempt_globs=EXEMPT_GLOBS):
    """Sorted paths in my_paths that also appear in other_prs_paths, minus exempt files."""
    others = set(other_prs_paths)
    return sorted(
        p for p in set(my_paths) if p in others and not matches(p, exempt_globs)
    )


def overlaps_by_pr(my_paths, others, exempt_globs=EXEMPT_GLOBS):
    """Map other PR number to the paths shared with my_paths. `others` is {number: paths}."""
    result = {}
    for number, paths in others.items():
        shared = overlap(my_paths, paths, exempt_globs)
        if shared:
            result[int(number)] = shared
    return result


def render_comment(by_pr):
    lines = [
        COMMENT_MARKER,
        "This pull request changes files that other open pull requests also change. "
        "Coordinate the order of merging to avoid conflicts. "
        "Exempt files (changelog, gate docs) are not listed.",
        "",
    ]
    for number in sorted(by_pr):
        lines.append(f"- #{number}")
        lines.extend(f"  - `{p}`" for p in by_pr[number])
    lines += ["", "This comment is updated in place on every run."]
    return "\n".join(lines)


def render_no_overlap():
    return f"{COMMENT_MARKER}\nNo overlapping files with other open pull requests."


def render_failure(areas):
    names = ", ".join(sorted(areas))
    return (
        f"This pull request touches risky paths ({names}) and has no `{REVIEWED_LABEL}` label.\n"
        "Risky paths need an independent review before merge. How to proceed:\n"
        "  1. Ask for an independent review covering the areas above "
        "(what each area must cover is in docs/review-policy.md).\n"
        f"  2. The repository owner applies the `{REVIEWED_LABEL}` label when that review is done.\n"
        "  3. This check re-runs when the label is added.\n"
        "Pushing new commits after the label does not remove it; re-review material changes."
    )


def _gh(args, stdin=None):
    return subprocess.run(
        ["gh", *args], input=stdin, capture_output=True, text=True, check=True
    ).stdout


def pr_files(repo, number):
    # --slurp preserves each paginated API array; JSON parsing also preserves unusual path names.
    pages = json.loads(_gh(
        ["api", "--paginate", "--slurp", f"repos/{repo}/pulls/{number}/files"]
    ))
    paths = set()
    for page in pages:
        for item in page:
            paths.add(item["filename"])
            if item.get("previous_filename"):
                paths.add(item["previous_filename"])
    return sorted(paths)


def cmd_risk(args):
    paths = pr_files(args.repo, args.pr) if args.pr else _read_lines(args.paths_file)
    labels = json.loads(args.labels_json)
    areas = classify_risk(paths)
    print(f"risk areas: {', '.join(sorted(areas)) or 'none'}")
    if required_label_ok(labels, areas):
        return 0
    print(f"::error title=Risky paths need the reviewed label::{render_failure(areas).splitlines()[0]}")
    print(render_failure(areas))
    return 1


def _read_lines(path):
    with open(path, encoding="utf-8") as f:
        return [line.strip() for line in f if line.strip()]


def _upsert_comment(repo, number, body, create):
    pages = json.loads(_gh(
        ["api", "--paginate", "--slurp", f"repos/{repo}/issues/{number}/comments"]
    ))
    ids = [str(comment["id"]) for page in pages for comment in page
           if comment.get("user", {}).get("login") == COMMENT_AUTHOR
           and comment["user"].get("type") == "Bot"
           and (comment.get("body") or "").startswith(COMMENT_MARKER)]
    if ids:
        _gh(["api", "-X", "PATCH", f"repos/{repo}/issues/comments/{ids[0]}", "-F", "body=@-"], body)
    elif create:
        _gh(["api", "-X", "POST", f"repos/{repo}/issues/{number}/comments", "-F", "body=@-"], body)


def cmd_claims(args):
    repo, number = args.repo, args.pr
    mine = pr_files(repo, number)
    out = _gh(
        ["api", "--paginate", f"repos/{repo}/pulls?state=open&per_page=100", "--jq",
         ".[] | {number}"]
    )
    listing = [json.loads(line) for line in out.splitlines() if line]
    others = {}
    for item in listing[:MAX_OPEN_PRS]:
        if item["number"] != number:
            others[item["number"]] = pr_files(repo, item["number"])
    by_pr = overlaps_by_pr(mine, others)
    if args.dry_run:
        print(json.dumps({str(k): v for k, v in by_pr.items()}, indent=2))
        return 0
    if by_pr:
        _gh(["api", "-X", "POST", f"repos/{repo}/issues/{number}/labels", "-f", f"labels[]={OVERLAP_LABEL}"])
        _upsert_comment(repo, number, render_comment(by_pr), create=True)
    else:
        # A missing label needs no DELETE. Other API errors must stay failures, not clean results.
        pages = json.loads(_gh(["api", "--paginate", "--slurp", f"repos/{repo}/issues/{number}/labels"]))
        if any(label["name"] == OVERLAP_LABEL for page in pages for label in page):
            _gh(["api", "-X", "DELETE", f"repos/{repo}/issues/{number}/labels/{OVERLAP_LABEL}"])
        _upsert_comment(repo, number, render_no_overlap(), create=False)
    print(f"overlapping pull requests: {sorted(by_pr) or 'none'}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)
    risk = sub.add_parser("risk", help="fail when risky paths lack the reviewed label")
    risk.add_argument("--repo")
    risk.add_argument("--pr", type=int)
    risk.add_argument("--paths-file", help="one changed path per line, instead of --pr")
    risk.add_argument("--labels-json", required=True, help='JSON list of label names')
    risk.set_defaults(func=cmd_risk)
    claims = sub.add_parser("claims", help="label and comment on overlap with open PRs")
    claims.add_argument("--repo", required=True)
    claims.add_argument("--pr", type=int, required=True)
    claims.add_argument("--dry-run", action="store_true", help="print overlaps, change nothing")
    claims.set_defaults(func=cmd_claims)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
