#!/usr/bin/env python3
"""Main guard: react when a push to main fails a required check.

The decision is a pure function (`decide`) so the whole table is unit tested. The CLI gathers the
facts with `gh` and `git`, asks `decide`, and then either does nothing, opens a revert pull request
plus a tracking issue, or only opens/updates the tracking issue. It never pushes to main: the only
ref it pushes is `revert/<short-sha>`.

Standard library only. See docs/main-guard.md.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from typing import NamedTuple

NONE = "NONE"
REVERT_PR = "REVERT_PR"
ISSUE_ONLY = "ISSUE_ONLY"

# Branch protection's required checks (tools/branch-protection.json).
REQUIRED_CHECKS = frozenset(
    {
        "gate (ubuntu-latest)",
        "gate (ubuntu-24.04-arm)",
        "secret-scan",
        "typos",
        "commit-messages",
        "msrv",
    }
)

# A job that "failed" only because the runner broke, not because the code did.
INFRA_STEPS = frozenset({"Set up job", "Initialize containers", "Complete job"})
NON_FAILURE_JOB_CONCLUSIONS = frozenset({"cancelled", "skipped", "timed_out", "neutral", "success"})

BOT_NAMES = ("dependabot[bot]", "github-merge-queue[bot]")
LABEL = "main-red"
ISSUE_TITLE = "main is red"


class Decision(NamedTuple):
    action: str
    reason: str


def _is_real_failure(name, jobs_by_name):
    """A required check counts only when its job genuinely failed (not infra, cancelled, timed out)."""
    job = jobs_by_name.get(name)
    if job is None:
        # No job detail: trust the caller's claim that the check failed.
        return True
    if job.get("conclusion") in NON_FAILURE_JOB_CONCLUSIONS:
        return False
    if job.get("infra"):
        return False
    return True


def decide(
    run_conclusion,
    failed_required_checks,
    failed_jobs_info,
    head_commit_is_revert,
    already_open_revert,
    *,
    bot_only=False,
    culprit_known=True,
):
    """Return a Decision(action, reason).

    run_conclusion: conclusion of the workflow run ("failure", "cancelled", ...).
    failed_required_checks: names of failed checks; only REQUIRED_CHECKS count.
    failed_jobs_info: list of dicts {name, conclusion, infra} for the failed jobs.
    head_commit_is_revert: the commit that would be reverted is itself a revert.
    already_open_revert: a revert PR for that sha already exists.
    bot_only: every candidate commit came from dependabot or the merge queue.
    culprit_known: a single offending commit could be determined.
    """
    if run_conclusion != "failure":
        return Decision(NONE, f"run concluded {run_conclusion!r}, not a failure")
    required = sorted(set(failed_required_checks) & REQUIRED_CHECKS)
    if not required:
        return Decision(NONE, "no required check failed (non-required, e.g. macOS-only, failure)")
    jobs_by_name = {j.get("name"): j for j in (failed_jobs_info or [])}
    real = [c for c in required if _is_real_failure(c, jobs_by_name)]
    if not real:
        return Decision(NONE, "required check failures are infrastructure, cancelled or timed out")
    names = ", ".join(real)
    if head_commit_is_revert:
        return Decision(ISSUE_ONLY, f"{names} failed on a revert commit; never revert a revert")
    if already_open_revert:
        return Decision(ISSUE_ONLY, f"{names} failed; a revert for this commit already exists")
    if bot_only:
        return Decision(ISSUE_ONLY, f"{names} failed on a dependabot or merge-queue change")
    if not culprit_known:
        return Decision(ISSUE_ONLY, f"{names} failed; no single offending commit could be determined")
    return Decision(REVERT_PR, f"{names} failed on main")


def is_revert_subject(subject):
    s = subject.strip()
    return bool(re.match(r'^(Revert\b|revert(\(.*?\))?!?:)', s))


def crates_in_log(log):
    """Crate names mentioned on error-looking lines of a failed job log."""
    found = set()
    for line in log.splitlines():
        low = line.lower()
        if not any(k in low for k in ("error", "failed", "panicked", "could not compile")):
            continue
        found.update(re.findall(r"\blattice(?:[-_][a-z0-9]+)*\b|\bxtask\b", line))
    return {c.replace("_", "-") for c in found}


def pick_culprit(commits_files, crates):
    """commits_files: ordered {sha: [paths]}. Return the sha, or None when not determinable."""
    if len(commits_files) == 1:
        return next(iter(commits_files))
    if not crates:
        return None
    prefixes = tuple(f"crates/{c}/" for c in crates) + tuple(f"{c}/" for c in crates)
    hits = [sha for sha, files in commits_files.items() if any(f.startswith(prefixes) for f in files)]
    return hits[0] if len(hits) == 1 else None


def infra_only(job):
    """True when every failed step is a runner-level step."""
    failed = [s["name"] for s in job.get("steps", []) if s.get("conclusion") == "failure"]
    return bool(failed) and all(n in INFRA_STEPS or n.startswith("Post ") for n in failed)


# ---------------------------------------------------------------- side effects

def run(cmd, check=True, capture=True):
    return subprocess.run(cmd, check=check, text=True, capture_output=capture).stdout


def gh_json(args):
    return json.loads(run(["gh", *args]))


def short(sha):
    return sha[:8]


def previous_main_sha(repo, run_id, created_at):
    runs = gh_json(
        ["api", f"repos/{repo}/actions/runs?branch=main&event=push&per_page=50"]
    )["workflow_runs"]
    this = [r for r in runs if r["id"] == run_id]
    name = this[0]["name"] if this else None
    older = [r for r in runs if r["name"] == name and r["created_at"] < created_at]
    older.sort(key=lambda r: r["created_at"], reverse=True)
    return older[0]["head_sha"] if older else None


def push_commits(repo, run_id, head_sha, created_at):
    """Commits in the push, oldest first. Falls back to the head commit alone."""
    prev = previous_main_sha(repo, run_id, created_at)
    rng = f"{prev}..{head_sha}" if prev else f"{head_sha}^..{head_sha}"
    out = run(["git", "rev-list", "--reverse", "--no-merges", rng], check=False)
    shas = out.split()
    return shas or [head_sha]


def commit_info(sha):
    fmt = run(["git", "log", "-1", "--format=%an%n%cn%n%s", sha]).splitlines()
    files = run(["git", "diff-tree", "--no-commit-id", "--name-only", "-r", sha]).splitlines()
    return {"author": fmt[0], "committer": fmt[1], "subject": fmt[2] if len(fmt) > 2 else "", "files": files}


def failed_job_logs(repo, jobs):
    logs = []
    for j in jobs:
        out = run(["gh", "api", f"repos/{repo}/actions/jobs/{j['id']}/logs"], check=False)
        logs.append(out)
    return "\n".join(logs)


def revert_message(sha, subject):
    base = re.sub(r'^Revert "(.*)"$', r"\1", subject)
    subj = f"revert: {base}"
    if len(subj) > 72:
        subj = subj[:71].rstrip() + "."
    return f"{subj}\n\nThis reverts commit {sha}.\n"


def do_revert(sha, subject, branch):
    run(["git", "fetch", "origin", "main"])
    run(["git", "switch", "-c", branch, "origin/main"])
    run(["git", "revert", "--no-edit", sha])
    msg = revert_message(sha, subject)
    subprocess.run(["git", "commit", "--amend", "-F", "-"], input=msg, text=True, check=True, capture_output=True)
    # Explicit refspec: this tool never pushes main.
    run(["git", "push", "origin", f"{branch}:refs/heads/{branch}"])


def ensure_label(repo):
    run(["gh", "label", "create", LABEL, "--repo", repo, "--color", "B60205",
         "--description", "main failed a required check"], check=False)


def pr_body(check, run_url, sha, subject):
    return (
        f"Automatic revert of {sha} (`{subject}`).\n\n"
        f"Failing required check: {check}\n"
        f"Run: {run_url}\n\n"
        "main was red after this commit. Merge this PR if the revert is right, or close it and fix forward. "
        "Nothing was pushed to main. See docs/main-guard.md.\n"
    )


def upsert_issue(repo, title_suffix, body):
    ensure_label(repo)
    existing = gh_json(
        ["issue", "list", "--repo", repo, "--label", LABEL, "--state", "open", "--json", "number,title"]
    )
    existing = [i for i in existing if i["title"].startswith(ISSUE_TITLE)]
    if existing:
        num = existing[0]["number"]
        run(["gh", "issue", "comment", str(num), "--repo", repo, "--body", body])
        return f"commented on #{num}"
    run(["gh", "issue", "create", "--repo", repo, "--title", f"{ISSUE_TITLE}: {title_suffix}",
         "--label", LABEL, "--label", "type: bug", "--body", body])
    return "opened tracking issue"


def existing_revert(repo, branch):
    prs = gh_json(["pr", "list", "--repo", repo, "--state", "all", "--head", branch, "--json", "number"])
    return bool(prs)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--run-id", type=int, required=True)
    ap.add_argument("--dry-run", action="store_true", help="decide and print, change nothing")
    args = ap.parse_args(argv)
    repo = args.repo

    info = gh_json(["api", f"repos/{repo}/actions/runs/{args.run_id}"])
    if info.get("head_branch") != "main" or info.get("event") != "push":
        print("not a push run on main; nothing to do")
        return 0
    conclusion = info.get("conclusion")
    head = info["head_sha"]
    run_url = info["html_url"]

    jobs = gh_json(["api", f"repos/{repo}/actions/runs/{args.run_id}/jobs?per_page=100"])["jobs"]
    failed = [j for j in jobs if j.get("conclusion") == "failure"]
    jobs_info = [
        {"name": j["name"], "conclusion": j["conclusion"], "infra": infra_only(j)} for j in failed
    ]
    failed_names = [j["name"] for j in failed]

    commits = push_commits(repo, args.run_id, head, info["created_at"])
    cinfo = {s: commit_info(s) for s in commits}
    crates = crates_in_log(failed_job_logs(repo, failed)) if len(commits) > 1 else set()
    culprit = pick_culprit({s: cinfo[s]["files"] for s in commits}, crates)
    target = culprit or head
    tinfo = cinfo[target]
    branch = f"revert/{short(target)}"
    bot_only = all(
        any(b in cinfo[s]["author"] or b in cinfo[s]["committer"] for b in BOT_NAMES) for s in commits
    )
    d = decide(
        conclusion,
        failed_names,
        jobs_info,
        is_revert_subject(tinfo["subject"]),
        existing_revert(repo, branch) if conclusion == "failure" else False,
        bot_only=bot_only,
        culprit_known=culprit is not None,
    )
    print(f"decision: {d.action}: {d.reason}")
    if d.action == NONE:
        return 0
    check = ", ".join(sorted(set(failed_names) & REQUIRED_CHECKS))
    issue_body = (
        f"Required check failed on main: {check}\n\nRun: {run_url}\nCommit: {target}\n"
        f"Decision: {d.action} ({d.reason}).\n"
    )
    if args.dry_run:
        print(f"dry run: would {d.action} on {short(target)}; no changes made")
        return 0
    if d.action == REVERT_PR:
        do_revert(target, tinfo["subject"], branch)
        ensure_label(repo)
        url = run(["gh", "pr", "create", "--repo", repo, "--base", "main", "--head", branch,
                   "--title", revert_message(target, tinfo["subject"]).splitlines()[0],
                   "--label", LABEL, "--body", pr_body(check, run_url, target, tinfo["subject"])]).strip()
        issue_body += f"Revert PR: {url}\n"
        print(f"opened {url}")
    print(upsert_issue(repo, short(target), issue_body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
