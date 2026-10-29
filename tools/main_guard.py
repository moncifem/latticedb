#!/usr/bin/env python3
"""Conservative main-red proposals. Default is dry-run; never writes main or changes settings."""

import argparse
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path
from typing import NamedTuple

REPOSITORY = "moncifem/latticedb"
WORKFLOW_PATH = ".github/workflows/pr.yml"
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
BOT_LOGINS = frozenset({"dependabot[bot]", "github-merge-queue[bot]"})
LABEL = "main-red"
NONE, REVERT_PR, ISSUE_ONLY = "NONE", "REVERT_PR", "ISSUE_ONLY"
MAX_PAGES = 10
VALIDATION_STEPS = {
    "gate (ubuntu-latest)": {
        "Run cargo xtask ci",
        "engine without row features",
        "scorecard evidence semantics and real ledger ingestion",
    },
    "gate (ubuntu-24.04-arm)": {"Run cargo xtask test-portable"},
    "msrv": {"Run cargo check --workspace"},
    "typos": {"Run typos"},
    "commit-messages": {"lint every commit subject in the PR"},
}
INFRA = re.compile(
    r"failed to (?:download|fetch)|could not resolve host|connection (?:reset|timed out)|HTTP (?:429|502|503|504)|rate limit exceeded|no space left on device|lost communication|runner.*(?:shutdown|lost)|signal: 9|SIGKILL|exit code 137",
    re.I,
)
CODE = re.compile(
    r'error\[E\d+\]:|panicked at [^\n]*(?:crates|xtask|tests)/[^\n]+:\d+|error: "[^"\n]+" should be "',
    re.I,
)
SOURCE_ERROR = re.compile(
    r"error:[^\n]+[\s\S]*?--> [^\n]*(?:crates|xtask|tests)/[^\n]+\.rs:\d+"
)
# These changes need explicit storage/format or guard-control review, not an automatic proposal.
MANUAL_PATHS = (
    "formats/",
    "docs/rfc/",
    "docs/adr/",
    "tools/main_guard.py",
    "tools/test_main_guard.py",
    ".github/workflows/main-guard.yml",
    ".github/workflows/pr.yml",
    "crates/lattice-core/src/admission.rs",
    "crates/lattice-core/src/record.rs",
    "crates/lattice-core/src/log.rs",
    "crates/lattice-engines/src/typed.rs",
    "crates/lattice-engines/src/catalog.rs",
)


class EvidenceError(RuntimeError):
    pass


class StaleRun(EvidenceError):
    pass


class Decision(NamedTuple):
    action: str
    reason: str


def command(args, *, cwd=None, input=None, env=None):
    try:
        result = subprocess.run(
            args,
            cwd=cwd,
            input=input,
            env=env,
            text=True,
            capture_output=True,
            timeout=90,
            check=True,
        )
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as error:
        # Do not copy logs, credentials, original commit identity or arbitrary stderr into issues.
        raise EvidenceError(
            f"{args[0]} command failed; evidence unavailable"
        ) from error
    if len(result.stdout) > 8 * 1024 * 1024:
        raise EvidenceError("command output exceeds evidence parser limit")
    return result.stdout


def git(*args, cwd=None, input=None, env=None):
    return command(
        [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!gh auth git-credential",
            *args,
        ],
        cwd=cwd,
        input=input,
        env=env,
    )


def api(path, method="GET", fields=None):
    args = ["gh", "api", "--method", method, path]
    payload = None
    if fields is not None:
        args.extend(["--input", "-"])
        payload = json.dumps(fields)
    data = command(args, input=payload)
    if not data.strip():
        return None
    try:
        return json.loads(data)
    except json.JSONDecodeError as error:
        raise EvidenceError("invalid API JSON") from error


def pages(path, key=None):
    values = []
    for page in range(1, MAX_PAGES + 1):
        data = api(f"{path}{'&' if '?' in path else '?'}per_page=100&page={page}")
        items = data.get(key) if key and isinstance(data, dict) else data
        if not isinstance(items, list):
            raise EvidenceError("missing paginated API evidence")
        values.extend(items)
        if len(items) < 100:
            if key and data.get("total_count", len(values)) != len(values):
                raise EvidenceError("incomplete API evidence")
            return values
    raise EvidenceError("API pagination exceeds bounded collection")


def full_sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


def verified_run(run, workflow, run_id):
    if (
        not isinstance(run, dict)
        or not isinstance(workflow, dict)
        or type(run_id) is not int
        or run_id <= 0
    ):
        return False
    return (
        type(workflow.get("id")) is int
        and workflow["id"] > 0
        and run.get("id") == run_id
        and run.get("workflow_id") == workflow.get("id")
        and workflow.get("path") == WORKFLOW_PATH
        and run.get("path") == WORKFLOW_PATH
        and run.get("repository", {}).get("full_name") == REPOSITORY
        and run.get("head_repository", {}).get("full_name") == REPOSITORY
        and run.get("event") == "push"
        and run.get("head_branch") == "main"
        and run.get("status") == "completed"
        and full_sha(run.get("head_sha"))
        and type(run.get("run_attempt")) is int
        and run["run_attempt"] > 0
    )


def is_revert_subject(subject):
    return bool(re.match(r"^(Revert\b|revert(?:\([^\n]*?\))?!?:)", subject.strip()))


def classify_job(job, log):
    if not isinstance(job, dict) or not isinstance(job.get("steps", []), list):
        return "unknown"
    if any(not isinstance(step, dict) for step in job.get("steps", [])):
        return "unknown"
    conclusion = job.get("conclusion")
    if conclusion in {"success", "cancelled", "skipped", "timed_out", "neutral"}:
        return "none"
    if conclusion != "failure":
        return "unknown"
    failed = [
        s.get("name") for s in job.get("steps", []) if s.get("conclusion") == "failure"
    ]
    if not failed or not isinstance(log, str):
        return "unknown"
    code = bool(CODE.search(log) or SOURCE_ERROR.search(log))
    infra = bool(INFRA.search(log))
    if infra:
        return "unknown" if code else "infra"
    known = VALIDATION_STEPS.get(job.get("name"), set())
    if job.get("name") == "secret-scan":
        known = {
            name
            for name in failed
            if isinstance(name, str)
            and name.startswith("Run gitleaks/gitleaks-action@")
        }
        code = bool(re.search(r"(?:leaks found|found \d+ leaks)", log, re.I))
    if all(name in known for name in failed) and code:
        return "code"
    return "unknown"


def decide(
    conclusion,
    failures,
    *,
    culprit_known=False,
    is_revert=False,
    duplicate=False,
    bot=False,
    manual=False,
    incident=False,
):
    if conclusion != "failure":
        return Decision(NONE, "run is not a completed failure")
    relevant = {
        name: state for name, state in failures.items() if name in REQUIRED_CHECKS
    }
    if not any(state in {"code", "unknown"} for state in relevant.values()):
        return Decision(
            NONE,
            "no code/unknown failure in required checks; optional/infra/cancelled ignored",
        )
    if any(state == "unknown" for state in relevant.values()):
        return Decision(
            ISSUE_ONLY, "required-check evidence or failure classification is unknown"
        )
    if not culprit_known:
        return Decision(
            ISSUE_ONLY,
            "no verified single change from an observed green main predecessor",
        )
    if is_revert or duplicate or bot or manual or incident:
        return Decision(
            ISSUE_ONLY,
            "revert/bot/duplicate/active incident/format-or-control change requires manual review",
        )
    return Decision(
        REVERT_PR,
        "required code-validation failure after one observed green predecessor",
    )


class Guard:
    def __init__(self, repo, run_id):
        if repo != REPOSITORY:
            raise EvidenceError("repository namespace is not authorized")
        policy = json.loads(Path("tools/branch-protection.json").read_text())
        if (
            set(policy.get("required_status_checks", {}).get("contexts", []))
            != REQUIRED_CHECKS
        ):
            raise EvidenceError(
                "declared required-check policy changed; review guard configuration"
            )
        origin = git("config", "--get", "remote.origin.url").strip()
        if origin not in {
            f"https://github.com/{repo}",
            f"https://github.com/{repo}.git",
            f"git@github.com:{repo}.git",
        }:
            raise EvidenceError("origin does not match the authorized repository")
        self.repo, self.run_id = repo, run_id
        self.prefix = f"repos/{repo}"
        self.workflow = api(f"{self.prefix}/actions/workflows/pr.yml")
        self.run = api(f"{self.prefix}/actions/runs/{run_id}")
        if not verified_run(self.run, self.workflow, run_id):
            raise StaleRun("not the verified completed pr workflow push/main run")
        self.head = self.run["head_sha"]
        self.branch = f"revert/{self.head}"
        self.url = f"https://github.com/{repo}/actions/runs/{run_id}"
        self.marker = f"<!-- main-guard:{self.head} -->"
        self.run_marker = f"<!-- main-guard-run:{run_id}:{self.run['run_attempt']} -->"
        self.published_candidate = None
        self.baseline = None
        self.assert_current()

    def assert_current(self):
        git("fetch", "--no-tags", "origin", "main")
        current = git("rev-parse", "refs/remotes/origin/main").strip()
        remote = git("ls-remote", "--exit-code", "origin", "refs/heads/main").split()
        observed = api(f"{self.prefix}/actions/runs/{self.run_id}")
        if (
            current != self.head
            or not remote
            or remote[0] != self.head
            or not verified_run(observed, self.workflow, self.run_id)
            or observed.get("run_attempt") != self.run["run_attempt"]
            or observed.get("conclusion") != self.run.get("conclusion")
            or observed.get("head_sha") != self.head
        ):
            raise StaleRun(
                "main or run attempt changed; no further mutation authorized"
            )
        if self.baseline is not None:
            baseline = api(f"{self.prefix}/actions/runs/{self.baseline['id']}")
            if (
                not verified_run(baseline, self.workflow, self.baseline["id"])
                or baseline.get("conclusion") != "success"
                or baseline.get("head_sha") != self.baseline["head_sha"]
                or baseline.get("run_attempt") != self.baseline["run_attempt"]
            ):
                raise StaleRun("green predecessor proof changed")
            if not self.predecessor():
                raise StaleRun("workflow history or newer success changed admission")
        if self.published_candidate is not None:
            candidate = git(
                "ls-remote", "--exit-code", "origin", f"refs/heads/{self.branch}"
            ).split()
            if not candidate or candidate[0] != self.published_candidate:
                raise StaleRun(
                    "candidate branch changed; no further mutation authorized"
                )

    def mutation(self, path, method="POST", fields=None):
        self.assert_current()
        return api(path, method, fields)

    def duplicate(self):
        remote = git("ls-remote", "origin", f"refs/heads/{self.branch}").split()
        if remote:
            return True
        prs = pages(f"{self.prefix}/pulls?state=all&head=moncifem:{self.branch}")
        return any(
            p.get("head", {}).get("ref") == self.branch
            and p.get("head", {}).get("repo", {}).get("full_name") == self.repo
            and p.get("base", {}).get("ref") == "main"
            for p in prs
        )

    def predecessor(self):
        parents = git("rev-list", "--parents", "-n", "1", self.head).split()
        if len(parents) != 2:
            return False
        runs = pages(
            f"{self.prefix}/actions/workflows/{self.workflow['id']}/runs?branch=main&event=push",
            "workflow_runs",
        )
        if any(not isinstance(run, dict) for run in runs):
            return False
        current = [r for r in runs if r.get("id") == self.run_id]
        if len(current) != 1 or not self.run.get("created_at"):
            return False
        if any(
            r.get("id") != self.run_id and r.get("created_at") == self.run["created_at"]
            for r in runs
        ):
            return False
        if any(
            verified_run(r, self.workflow, r.get("id"))
            and r.get("created_at", "") > self.run["created_at"]
            and r.get("conclusion") == "success"
            for r in runs
        ):
            return False
        older = [r for r in runs if r.get("created_at", "") < self.run["created_at"]]
        if not older:
            return False
        previous = max(older, key=lambda r: r["created_at"])
        if sum(r.get("created_at") == previous["created_at"] for r in older) != 1:
            return False
        if not verified_run(previous, self.workflow, previous.get("id")):
            return False
        # No fallback to HEAD: a multi-commit or unknown delta only produces an issue.
        known = (
            previous.get("conclusion") == "success"
            and previous.get("head_sha") == parents[1]
        )
        if known:
            self.baseline = previous
        return known

    def failures(self):
        jobs = pages(
            f"{self.prefix}/actions/runs/{self.run_id}/attempts/{self.run['run_attempt']}/jobs",
            "jobs",
        )
        if any(not isinstance(job, dict) for job in jobs):
            raise EvidenceError("malformed job evidence")
        result = {}
        self.failure_paths = {}
        for name in REQUIRED_CHECKS:
            matches = [j for j in jobs if j.get("name") == name]
            if len(matches) != 1:
                result[name] = "unknown"
                continue
            job = matches[0]
            log = None
            if job.get("conclusion") == "failure":
                try:
                    log = command(
                        [
                            "gh",
                            "api",
                            f"{self.prefix}/actions/jobs/{int(job['id'])}/logs",
                        ]
                    )
                except (EvidenceError, KeyError, ValueError, TypeError):
                    pass
            result[name] = classify_job(job, log)
            if result[name] == "code":
                paths = set()
                for match in re.finditer(r"(?:-->|at )\s*([A-Za-z0-9_./-]+):\d+", log):
                    path = match[1]
                    rooted = re.search(r"(?:crates|xtask|tests)/.+", path)
                    paths.add(rooted[0] if rooted else path)
                self.failure_paths[name] = paths
        return result

    def active_incident(self):
        # Only this repository's full-SHA candidate namespace or exact tracking
        # markers count; similarly named foreign PRs are not our incidents.
        for pr in pages(f"{self.prefix}/pulls?state=open&base=main"):
            head = pr.get("head") or {}
            ref = head.get("ref", "")
            if (
                pr.get("state") == "open"
                and (pr.get("base") or {}).get("ref") == "main"
                and (head.get("repo") or {}).get("full_name") == REPOSITORY
                and re.fullmatch(r"revert/[0-9a-f]{40}", ref)
                and any(label.get("name") == LABEL for label in pr.get("labels", []))
            ):
                return True
        for issue in pages(f"{self.prefix}/issues?state=open&labels={LABEL}"):
            title = issue.get("title", "")
            match = re.fullmatch(r"main is red: ([0-9a-f]{40})", title)
            if (
                issue.get("state") == "open"
                and not issue.get("pull_request")
                and match
                and match[1] != self.head
                and f"<!-- main-guard:{match[1]} -->" in (issue.get("body") or "")
            ):
                return True
        return False

    def decision(self):
        failures = self.failures()
        preliminary = decide(self.run.get("conclusion"), failures)
        if preliminary.action == NONE:
            return preliminary, failures
        subject = git("log", "-1", "--format=%s", self.head).strip()
        files = git(
            "diff-tree", "--no-commit-id", "--name-only", "-z", "-r", self.head
        ).split("\0")
        commit = api(f"{self.prefix}/commits/{self.head}")
        if not isinstance(commit, dict) or commit.get("sha") != self.head:
            raise EvidenceError("commit provenance mismatch")
        bot = any(
            (commit.get(field) or {}).get("login") in BOT_LOGINS
            for field in ("author", "committer")
        )
        identity_known = all(
            isinstance((commit.get(field) or {}).get("login"), str)
            and (commit.get(field) or {})["login"]
            for field in ("author", "committer")
        )
        relevant_source = all(
            paths and set(files).intersection(paths)
            for paths in self.failure_paths.values()
        )
        return decide(
            self.run.get("conclusion"),
            failures,
            culprit_known=self.predecessor(),
            is_revert=is_revert_subject(subject),
            duplicate=self.duplicate(),
            incident=self.active_incident(),
            bot=bot,
            manual=not identity_known
            or not relevant_source
            or any(p and p.startswith(MANUAL_PATHS) for p in files),
        ), failures

    def tracking_issue(self, decision, failures):
        title = f"main is red: {self.head}"
        names = (
            ", ".join(
                sorted(
                    n for n, state in failures.items() if state in {"code", "unknown"}
                )
            )
            or "unknown"
        )
        body = (
            f"{self.marker}\n{self.run_marker}\nRequired failure: {names}\nRun: {self.url}\nExact head: {self.head}\n"
            f"Decision: {decision.action}: {decision.reason}.\n"
            "No main write or automatic merge is authorized. Revert proposals require review and fresh checks."
        )
        issues = pages(f"{self.prefix}/issues?state=all&labels={LABEL}")
        for issue in issues:
            if (
                not issue.get("pull_request")
                and issue.get("title") == title
                and self.marker in (issue.get("body") or "")
            ):
                if issue.get("state") != "closed" and self.run_marker not in (
                    issue.get("body") or ""
                ):
                    comments = pages(f"{self.prefix}/issues/{issue['number']}/comments")
                    if not any(
                        self.run_marker in (comment.get("body") or "")
                        for comment in comments
                    ):
                        self.mutation(
                            f"{self.prefix}/issues/{issue['number']}/comments",
                            fields={"body": body},
                        )
                return issue["number"]
        labels = pages(f"{self.prefix}/labels")
        if not any(label.get("name") == LABEL for label in labels):
            self.mutation(
                f"{self.prefix}/labels", fields={"name": LABEL, "color": "B60205"}
            )
        return self.mutation(
            f"{self.prefix}/issues",
            fields={"title": title, "body": body, "labels": [LABEL]},
        )["number"]

    def publish(self, decision, failures):
        if decision.action != REVERT_PR:
            self.baseline = None
        issue = self.tracking_issue(decision, failures)
        if decision.action != REVERT_PR:
            return
        if self.duplicate() or self.active_incident():
            return
        try:
            self.publish_candidate(decision)
        except StaleRun:
            raise
        except EvidenceError:
            # Never copy command stderr, credentials, or commit text into a report.
            self.mutation(
                f"{self.prefix}/issues/{issue}/comments",
                fields={
                    "body": "Candidate publication failed; inspect the workflow diagnostics and existing candidate ref before retrying. No automatic cleanup or main write was attempted."
                },
            )
            raise

    def publish_candidate(self, decision):
        self.assert_current()
        # A private worktree avoids sweeping another checkout's staged or untracked work.
        with tempfile.TemporaryDirectory(prefix="main-guard-") as directory:
            work = str(Path(directory) / "revert")
            git("worktree", "add", "--detach", work, self.head)
            try:
                self.assert_current()
                git("revert", "--no-commit", self.head, cwd=work)
                paths = [
                    p
                    for p in git(
                        "diff", "--cached", "--name-only", "-z", cwd=work
                    ).split("\0")
                    if p
                ]
                if not paths:
                    raise EvidenceError("revert has no staged patch")
                message = f"revert: restore the green predecessor of {self.head[:12]}\n\nThis reverts commit {self.head}.\n"
                self.assert_current()
                identity = {
                    **os.environ,
                    "GIT_AUTHOR_NAME": "Moncif",
                    "GIT_COMMITTER_NAME": "Moncif",
                    "GIT_AUTHOR_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com",
                    "GIT_COMMITTER_EMAIL": "41898282+github-actions[bot]@users.noreply.github.com",
                }
                git(
                    "commit",
                    "-F",
                    "-",
                    "--",
                    *paths,
                    cwd=work,
                    input=message,
                    env=identity,
                )
                candidate = git("rev-parse", "HEAD", cwd=work).strip()
                self.assert_current()
                if self.duplicate():
                    return
                git(
                    "push",
                    f"--force-with-lease=refs/heads/{self.branch}:",
                    "origin",
                    f"HEAD:refs/heads/{self.branch}",
                    cwd=work,
                )
                self.published_candidate = candidate
                self.assert_current()
                made = self.mutation(
                    f"{self.prefix}/pulls",
                    fields={
                        "title": message.splitlines()[0],
                        "head": self.branch,
                        "base": "main",
                        "draft": True,
                        "body": f"{self.marker}\nCandidate only; expected main {self.head}.\nRun: {self.url}\nRequired evidence: {decision.reason}\nReview storage/format safety and trigger fresh required CI with a human credential before merging.\nNo main branch write occurred.",
                    },
                )
                self.mutation(
                    f"{self.prefix}/issues/{int(made['number'])}/labels",
                    fields={"labels": [LABEL]},
                )
            finally:
                git("worktree", "remove", "--force", work)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--run-id", type=int, required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--apply",
        action="store_true",
        help="explicitly allow fenced issue/draft-PR publication",
    )
    mode.add_argument(
        "--dry-run", action="store_true", help="read-only decision (the default)"
    )
    args = parser.parse_args(argv)
    if args.run_id <= 0:
        parser.error("run-id must be positive")
    guard = None
    try:
        guard = Guard(args.repo, args.run_id)
        if guard.run.get("conclusion") != "failure":
            print("decision: NONE: run did not fail")
            return 0
        try:
            decision, failures = guard.decision()
        except EvidenceError:
            decision, failures = (
                Decision(
                    ISSUE_ONLY, "incomplete evidence; automatic attribution refused"
                ),
                {},
            )
        print(f"decision: {decision.action}: {decision.reason}")
        if not args.apply or decision.action == NONE:
            print("dry-run/no-action: no remote refs, labels, issues or PRs changed")
            return 0
        guard.publish(decision, failures)
        return 0
    except StaleRun as error:
        print(f"decision: NONE: {error}")
        return 0
    except EvidenceError as error:
        print(f"guard stopped: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
