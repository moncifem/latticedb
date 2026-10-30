#!/usr/bin/env python3
"""Serial merge queue for batches of pull requests (issue 350).

For each pull request, in order: bring it up to date when it is behind, wait for ALL checks on the
new head, re-run the failed jobs once (flakes, stale runs), merge when every check is green and every
required context passed, otherwise park it with the failing check names and go on. Parked pull
requests come back in the next round, so a fixed branch is picked up without a restart.

Safety rules, each covered by tests/test_merge_queue.py:
  * never merges unless every required context (tools/branch-protection.json) and every `--also-require`
    check (default risk-gate) passed on the exact head that is merged (`--match-head-commit`), and no
    check failed or is pending;
  * decides "behind the base" from the compare API (`behind_by`), not from mergeStateStatus alone,
    which GitHub often reports as UNKNOWN or BLOCKED for a branch that is behind;
  * an API error, an empty answer or malformed JSON is "unknown": wait and ask again, never a result;
  * never force-pushes and never uses `--admin`; the only branch change is `gh pr update-branch
    --rebase`, which GitHub performs on the server;
  * drafts, conflicting (DIRTY) pull requests, pull requests with the hold label, pull requests that
    do not target the base branch and Dependabot pull requests (unless listed by number) are skipped.

State is written after every decision: a JSON status file for machines and a human log. Pure functions
(parsing, readiness, check classification, ordering) have no I/O; everything that calls `gh` lives in
`GhApi` and `Queue`. Usage and the full option list: docs/merging.md.
"""

import argparse
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_REQUIRED_FILE = HERE / "branch-protection.json"
DEFAULT_BASE = "main"
DEFAULT_HOLD_LABEL = "hold"
# The owner's label after an independent review. The queue refuses a pull request without it, whatever
# paths it touches, because the risk-gate path list can miss a risky change (PR 412). Dependency updates
# are exempt; `--allow-unreviewed` switches the rule off.
DEFAULT_REVIEWED_LABEL = "reviewed"
DEPENDABOT_LOGINS = ("app/dependabot", "dependabot[bot]")
DEPENDENCY_TITLE_RE =re.compile(r"\A(?:build|chore)\(deps(?:-dev)?\)[:!]")
# Required by the queue itself even if branch protection does not list it: a red or missing risk gate
# (the owner's `reviewed` label on risky paths) must never be merged over.
DEFAULT_ALSO_REQUIRE = ("risk-gate",)
UNKNOWN_STATE_TRIES = 3
HEAD_RE = re.compile(r"\A[0-9a-f]{40}\Z")
RUN_RE = re.compile(r"/actions/runs/([0-9]+)")
# `gh pr checks` exits 1 when a check failed and 8 while checks are pending, with valid JSON on stdout.
CHECKS_OK_EXIT = (0, 1, 8)
FAILED_BUCKETS = ("fail", "cancel")
PASSED_BUCKETS = ("pass", "skipping")
VIEW_FIELDS = "state,isDraft,mergeStateStatus,headRefOid,headRefName,baseRefName,labels,title,author"
CHECK_FIELDS = "name,bucket,link,workflow,startedAt,completedAt"
MAX_ATTEMPTS = 6


# ---------------------------------------------------------------------------------------------
# Pure functions
# ---------------------------------------------------------------------------------------------

def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_json(text, kind):
    """Parse `text` as JSON of type `kind` (dict or list); anything else is None (unknown)."""
    if not text:
        return None
    try:
        value = json.loads(text)
    except ValueError:
        return None
    return value if isinstance(value, kind) else None


def parse_pr_view(text):
    """A well-formed `gh pr view` snapshot, or None. A snapshot without a full head SHA is unknown."""
    info = parse_json(text, dict)
    if info is None:
        return None
    head = info.get("headRefOid")
    if not isinstance(head, str) or not HEAD_RE.match(head):
        return None
    for key in ("state", "mergeStateStatus", "baseRefName"):
        if not isinstance(info.get(key), str):
            return None
    if not isinstance(info.get("isDraft"), bool):
        return None
    if not isinstance(info.get("labels", []), list):
        return None
    return info


def label_names(info):
    return {label.get("name") for label in info.get("labels") or [] if isinstance(label, dict)}


def is_bot_author(info):
    author = info.get("author") or {}
    login = str(author.get("login") or "").lower()
    return bool(author.get("is_bot")) or "dependabot" in login or login.endswith("[bot]")


def is_dependency_update(info):
    """Dependabot's own pull request: its exact login, a bot account, its branch prefix and title shape.

    The test is narrow on purpose. A human named `dependabot-fan`, another bot app, or any other title
    or branch does not qualify. The exemption also does not look at paths: a Dependabot pull request
    that touches `.github/**` still needs a green `risk-gate`, which is a required check."""
    author = info.get("author") or {}
    return (
        author.get("is_bot") is True
        and author.get("login") in DEPENDABOT_LOGINS
        and str(info.get("headRefName") or "").startswith("dependabot/")
        and bool(DEPENDENCY_TITLE_RE.match(str(info.get("title") or "")))
    )


def readiness(info, base=DEFAULT_BASE, hold_label=DEFAULT_HOLD_LABEL, listed=False,
              reviewed_label=DEFAULT_REVIEWED_LABEL):
    """Return (kind, reason). kind is "ready", "closed" (nothing left to do) or "skip".

    An empty `reviewed_label` turns the review requirement off."""
    if info["state"] != "OPEN":
        return "closed", info["state"].lower()
    if info["baseRefName"] != base:
        return "skip", f"base is {info['baseRefName']}, not {base}"
    if info["isDraft"]:
        return "skip", "draft"
    if info["mergeStateStatus"] == "DIRTY":
        return "skip", "conflicts with the base branch"
    if hold_label and hold_label in label_names(info):
        return "skip", f"has the {hold_label} label"
    if reviewed_label and reviewed_label not in label_names(info) and not is_dependency_update(info):
        return "skip", f"no {reviewed_label} label (not a dependency update)"
    if is_bot_author(info) and not listed:
        return "skip", "bot pull request not listed by number"
    return "ready", ""


def discovery_order(prs, hold_label=DEFAULT_HOLD_LABEL, include_bots=False):
    """Numbers of the open pull requests worth queueing, oldest number first."""
    chosen = []
    for pr in prs:
        if not isinstance(pr, dict) or not isinstance(pr.get("number"), int):
            continue
        if pr.get("isDraft"):
            continue
        if hold_label and hold_label in label_names(pr):
            continue
        if is_bot_author(pr) and not include_bots:
            continue
        chosen.append(pr["number"])
    return sorted(set(chosen))


def explicit_order(numbers):
    """The numbers as given, first occurrence wins."""
    seen, out = set(), []
    for n in numbers:
        if n not in seen:
            seen.add(n)
            out.append(n)
    return out


def required_contexts(config):
    """The required check names from a branch protection document, or None when unusable."""
    try:
        contexts = config["required_status_checks"]["contexts"]
    except (KeyError, TypeError):
        return None
    if not isinstance(contexts, list) or not contexts or not all(isinstance(c, str) and c for c in contexts):
        return None
    return sorted(set(contexts))


def latest_checks(checks):
    """One record per (workflow, name): the latest start wins; on a tie the worse bucket wins."""
    rank = {"fail": 3, "cancel": 3, "pending": 2}
    best = {}
    for check in checks:
        if not isinstance(check, dict) or not isinstance(check.get("name"), str):
            continue
        key = (check.get("workflow") or "", check["name"])
        stamp = check.get("startedAt") or ""
        score = (stamp, rank.get(check.get("bucket"), 0))
        if key not in best or score >= best[key][0]:
            best[key] = (score, check)
    return [check for _, check in best.values()]


def classify_checks(checks, required):
    """Return (verdict, names). Verdict: "unknown", "pending", "failed" or "green".

    Pending wins over failed so every check finishes before a decision is made. Green needs every
    check passed or skipped and every required context passed (a missing one is pending: its run may
    not have been registered yet; the per-PR timeout turns a never-arriving one into a park).
    """
    if not isinstance(checks, list) or not checks:
        return "unknown", []
    latest = latest_checks(checks)
    if not latest:
        return "unknown", []
    pending = sorted({c["name"] for c in latest if c.get("bucket") not in FAILED_BUCKETS + PASSED_BUCKETS})
    if pending:
        return "pending", pending
    failed = sorted({c["name"] for c in latest if c.get("bucket") in FAILED_BUCKETS})
    if failed:
        return "failed", failed
    passed = {c["name"] for c in latest if c.get("bucket") == "pass"}
    missing = sorted(set(required) - passed)
    if missing:
        return "pending", [f"required check not passed: {name}" for name in missing]
    return "green", []


def failed_run_ids(checks):
    """Workflow run ids behind the failed checks (a status check without a run has none)."""
    ids = set()
    for check in latest_checks(checks):
        if check.get("bucket") in FAILED_BUCKETS:
            match = RUN_RE.search(check.get("link") or "")
            if match:
                ids.add(match.group(1))
    return sorted(ids)


def mentions_conflict(text):
    return "conflict" in (text or "").lower()


# ---------------------------------------------------------------------------------------------
# gh access (the only place that runs a process)
# ---------------------------------------------------------------------------------------------

class GhApi:
    def __init__(self, repo=None, timeout=120):
        self.repo = repo
        self.timeout = timeout

    def run(self, args):
        cmd = ["gh", *args]
        if self.repo and args[:1] != ["api"]:
            cmd += ["-R", self.repo]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=self.timeout)
        except (OSError, subprocess.TimeoutExpired) as exc:
            return 127, "", str(exc)
        return r.returncode, r.stdout.strip(), r.stderr.strip()

    def view(self, n):
        rc, out, _ = self.run(["pr", "view", str(n), "--json", VIEW_FIELDS])
        return parse_pr_view(out) if rc == 0 else None

    def checks(self, n):
        rc, out, _ = self.run(["pr", "checks", str(n), "--json", CHECK_FIELDS])
        return parse_json(out, list) if rc in CHECKS_OK_EXIT else None

    def list_open(self, base):
        rc, out, _ = self.run(["pr", "list", "--state", "open", "--base", base, "--limit", "200",
                               "--json", "number,isDraft,author,labels"])
        return parse_json(out, list) if rc == 0 else None

    def behind_by(self, info):
        """Commits the head is behind its base, or None (unknown) on an API error or odd answer."""
        slug = self.repo or "{owner}/{repo}"
        rc, out, _ = self.run(["api", f"repos/{slug}/compare/{info['baseRefName']}...{info['headRefOid']}",
                               "--jq", ".behind_by"])
        if rc != 0 or not re.fullmatch(r"[0-9]+", out or ""):
            return None
        return int(out)

    def update_branch(self, n):
        rc, out, err = self.run(["pr", "update-branch", str(n), "--rebase"])
        return rc, f"{out}\n{err}".strip()

    def rerun_failed(self, run_id):
        rc, _, _ = self.run(["run", "rerun", str(run_id), "--failed"])
        return rc == 0

    def merge(self, n, head):
        rc, out, err = self.run(["pr", "merge", str(n), "--rebase", "--match-head-commit", head])
        return rc, f"{out}\n{err}".strip()


# ---------------------------------------------------------------------------------------------
# The queue
# ---------------------------------------------------------------------------------------------

class Options:
    def __init__(self, **kw):
        self.base = DEFAULT_BASE
        self.hold_label = DEFAULT_HOLD_LABEL
        self.reviewed_label = DEFAULT_REVIEWED_LABEL
        self.include_bots = False
        self.also_require = DEFAULT_ALSO_REQUIRE
        self.dry_run = False
        self.per_pr_timeout = 40 * 60.0
        self.poll = 30.0
        self.settle = 20.0
        self.rerun_grace = 180.0
        self.rounds = 6
        self.round_sleep = 120.0
        self.status_file = None
        self.log_file = None
        for key, value in kw.items():
            if not hasattr(self, key):
                raise TypeError(key)
            setattr(self, key, value)


class Queue:
    def __init__(self, api, required, opts, out=None):
        self.api = api
        self.required = sorted(set(required) | set(opts.also_require or ()))
        self.opts = opts
        self.out = out if out is not None else sys.stdout
        self.sleep = time.sleep
        self.clock = time.monotonic
        self.last_failed_checks = []
        self.state = {"started": now_iso(), "updated": now_iso(), "dry_run": opts.dry_run,
                      "round": 0, "current": None, "finished": False, "prs": {}}

    # -- output ---------------------------------------------------------------------------
    def log(self, msg):
        line = f"[{datetime.now(timezone.utc):%H:%M:%S}] {msg}"
        print(line, file=self.out, flush=True)
        if self.opts.log_file:
            try:
                fd = os.open(self.opts.log_file, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
                with os.fdopen(fd, "a", encoding="utf-8") as fh:
                    fh.write(line + "\n")
            except OSError as exc:
                # A logging failure must never abort the queue, least of all right after a merge.
                print(f"warning: cannot write the log ({exc})", file=sys.stderr)

    def write_status(self):
        self.state["updated"] = now_iso()
        path = self.opts.status_file
        if not path:
            return
        directory = os.path.dirname(os.path.abspath(path))
        tmp = None
        try:
            fd, tmp = tempfile.mkstemp(dir=directory, prefix=".status-", suffix=".tmp")
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                json.dump(self.state, fh, indent=2, sort_keys=True)
                fh.write("\n")
            os.replace(tmp, path)
        except OSError as exc:
            print(f"warning: cannot write the status file ({exc})", file=sys.stderr)
        finally:
            if tmp and os.path.exists(tmp):
                os.unlink(tmp)

    def record(self, n, state, reason="", head=None, failed=None):
        entry = {"state": state, "reason": reason, "at": now_iso(), "round": self.state["round"]}
        if head:
            entry["head"] = head
        if failed:
            entry["failed_checks"] = list(failed)
        self.state["prs"][str(n)] = entry
        self.write_status()
        suffix = f": {reason}" if reason else ""
        self.log(f"PR {n}: {state.upper()}{suffix}")

    # -- waiting --------------------------------------------------------------------------
    def wait_checks(self, n, head, deadline, rerun_names=None, grace_until=0.0):
        """Return (outcome, names). Outcome: green, failed, moved, timeout, gone.

        After a re-run the old failure is still reported until the new run registers, so a failure
        limited to the re-run names is ignored until `grace_until`.
        """
        while self.clock() < deadline:
            info = self.api.view(n)
            if info is None:
                self.sleep(self.opts.poll)
                continue
            if info["state"] != "OPEN":
                return "gone", [info["state"].lower()]
            if info["headRefOid"] != head:
                return "moved", []
            checks = self.api.checks(n)
            verdict, names = classify_checks(checks, self.required)
            if verdict == "green":
                return "green", []
            if verdict == "failed":
                self.last_failed_checks = checks
                stale = rerun_names and self.clock() < grace_until and set(names) <= set(rerun_names)
                if not stale:
                    return "failed", names
            self.sleep(self.opts.poll)
        return "timeout", []

    # -- one pull request -----------------------------------------------------------------
    def process(self, n, listed=False):
        """Return "merged", "closed", "parked" or "skipped"."""
        deadline = self.clock() + self.opts.per_pr_timeout
        reran = False
        self.state["current"] = n
        self.write_status()
        attempts = 0
        unknown_state = 0
        while attempts < MAX_ATTEMPTS:
            if self.clock() >= deadline:
                self.record(n, "parked", "timed out")
                return "parked"
            info = self.api.view(n)
            if info is None:
                self.log(f"PR {n}: API answer unknown, asking again")
                self.sleep(self.opts.poll)
                continue
            kind, reason = readiness(info, self.opts.base, self.opts.hold_label, listed, self.opts.reviewed_label)
            if kind == "closed":
                self.record(n, "closed", reason)
                return "closed"
            if kind == "skip":
                self.record(n, "skipped", reason, head=info["headRefOid"])
                return "skipped"
            head = info["headRefOid"]
            if info["mergeStateStatus"] == "UNKNOWN" and unknown_state < UNKNOWN_STATE_TRIES:
                # GitHub computes the merge state lazily: ask again before deciding anything.
                unknown_state += 1
                self.log(f"PR {n}: merge state UNKNOWN, asking again ({unknown_state}/{UNKNOWN_STATE_TRIES})")
                self.sleep(self.opts.poll)
                continue
            behind = self.api.behind_by(info)
            if behind is None:
                self.log(f"PR {n}: cannot tell whether the branch is behind (API), asking again")
                self.sleep(self.opts.poll)
                continue
            if behind > 0 or info["mergeStateStatus"] == "BEHIND":
                if self.opts.dry_run:
                    self.record(n, "dry-run", "behind the base: would update the branch", head=head)
                    return "skipped"
                attempts += 1
                self.record(n, "updating", f"behind the base by {behind}, updating the branch", head=head)
                rc, text = self.api.update_branch(n)
                if rc != 0 and mentions_conflict(text):
                    self.record(n, "skipped", "update-branch reports conflicts", head=head)
                    return "skipped"
                self.sleep(self.opts.settle)
                continue
            if self.opts.dry_run:
                return self.dry_run_report(n, head)
            self.record(n, "waiting", "waiting for checks", head=head)
            outcome, names = self.wait_checks(n, head, deadline)
            if outcome == "failed" and not reran:
                reran = True
                ids = failed_run_ids(self.last_failed_checks)
                joined = ", ".join(names)
                if not ids:
                    self.record(n, "parked", f"failed checks without a re-runnable run: {joined}",
                                head=head, failed=names)
                    return "parked"
                self.log(f"PR {n}: failed [{joined}], re-running runs {', '.join(ids)} once")
                if not all([self.api.rerun_failed(i) for i in ids]):
                    self.record(n, "parked", f"failed and the re-run was refused: {joined}",
                                head=head, failed=names)
                    return "parked"
                self.sleep(self.opts.settle)
                outcome, names = self.wait_checks(n, head, deadline, rerun_names=names,
                                                  grace_until=self.clock() + self.opts.rerun_grace)
            if outcome == "gone":
                self.record(n, "closed", names[0])
                return "closed"
            if outcome == "moved":
                self.log(f"PR {n}: head moved while waiting, starting over")
                attempts += 1
                continue
            if outcome == "timeout":
                self.record(n, "parked", "timed out waiting for checks", head=head)
                return "parked"
            if outcome == "failed":
                self.record(n, "parked", f"checks failed: {', '.join(names)}", head=head, failed=names)
                return "parked"
            result = self.merge_if_still_green(n, head)
            if result:
                return result
            attempts += 1
        self.record(n, "parked", "gave up after repeated retries")
        return "parked"

    def dry_run_report(self, n, head):
        verdict, names = classify_checks(self.api.checks(n), self.required)
        detail = {"green": "all checks green: would merge",
                  "pending": f"checks pending ({', '.join(names)}): would wait",
                  "failed": f"checks failed ({', '.join(names)}): would re-run once, then park",
                  "unknown": "check state unknown: would ask again"}[verdict]
        self.record(n, "dry-run", detail, head=head, failed=names if verdict == "failed" else None)
        return "skipped"

    def merge_if_still_green(self, n, head):
        """Re-read everything right before the merge. Return a result, or None to go round again."""
        info = self.api.view(n)
        if info is None or info["headRefOid"] != head:
            self.log(f"PR {n}: head changed before the merge, starting over")
            return None
        kind, reason = readiness(info, self.opts.base, self.opts.hold_label, True, self.opts.reviewed_label)
        if kind == "closed":
            self.record(n, "closed", reason)
            return "closed"
        if kind == "skip":
            self.record(n, "skipped", reason, head=head)
            return "skipped"
        behind = self.api.behind_by(info)
        if behind is None or behind > 0 or info["mergeStateStatus"] == "BEHIND":
            self.log(f"PR {n}: behind the base or unknown ({behind}) right before the merge, starting over")
            return None
        verdict, names = classify_checks(self.api.checks(n), self.required)
        if verdict != "green":
            self.log(f"PR {n}: checks no longer green ({verdict}: {', '.join(names)}), starting over")
            return None
        rc, text = self.api.merge(n, head)
        after = self.api.view(n)
        if after is not None and after["state"] == "MERGED":
            self.record(n, "merged", "", head=head)
            return "merged"
        shown = text.replace("\n", " ")[:160]
        self.log(f"PR {n}: merge not accepted (exit {rc}: {shown}), will retry")
        self.sleep(self.opts.poll)
        return None

    # -- rounds ---------------------------------------------------------------------------
    def run_round(self, numbers, listed):
        results = {}
        for n in numbers:
            results[n] = self.process(n, listed=(n in listed or self.opts.include_bots))
        self.state["current"] = None
        self.write_status()
        return results

    def discover(self):
        prs = self.api.list_open(self.opts.base)
        if prs is None:
            return None
        return discovery_order(prs, self.opts.hold_label, self.opts.include_bots)

    def run(self, explicit, once):
        """Run rounds until nothing is left or the round limit is reached. Return the left-over."""
        listed = set(explicit)
        remaining = list(explicit)
        left = []
        for rnd in range(1, (1 if once else self.opts.rounds) + 1):
            self.state["round"] = rnd
            if not explicit:
                found = self.discover()
                if found is None:
                    self.log("could not list open pull requests (API), will try again")
                    found = remaining
                remaining = found
            if not remaining:
                self.log(f"round {rnd}: no candidates")
                left = []
                break
            self.log(f"round {rnd}: {' '.join(str(n) for n in remaining)}")
            self.write_status()
            results = self.run_round(remaining, listed)
            left = [n for n in remaining if results[n] in ("parked", "skipped")]
            remaining = left
            if not left or self.opts.dry_run:
                break
            if rnd < (1 if once else self.opts.rounds):
                self.log(f"round {rnd} left {' '.join(str(n) for n in left)}; next round in "
                         f"{self.opts.round_sleep:.0f}s")
                self.sleep(self.opts.round_sleep)
        self.state["finished"] = True
        self.state["left"] = left
        self.write_status()
        self.log(f"queue finished; not merged: {left if left else 'none'}")
        return left


# ---------------------------------------------------------------------------------------------
# Command line
# ---------------------------------------------------------------------------------------------

def default_paths():
    base = Path(os.environ.get("TMPDIR") or tempfile.gettempdir()) / "lattice-merge-queue"
    return base / "status.json", base / "queue.log"


def private_dir(path):
    """Create `path` (mode 0700) or accept an existing real directory owned by this user."""
    path = Path(path)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
        raise OSError(f"{path} is not a directory owned by you (symlink or foreign owner); refusing to use it")
    if st.st_mode & 0o077:
        os.chmod(path, 0o700)


def build_parser():
    p = argparse.ArgumentParser(description="Serial merge queue: update, wait for all checks, merge, park failures.")
    p.add_argument("prs", nargs="*", type=int, metavar="PR", help="pull request numbers in merge order; "
                   "none means every ready open pull request, lowest number first")
    p.add_argument("--once", action="store_true", help="one pass over the queue, no retry rounds")
    p.add_argument("--rounds", type=int, default=6, help="rounds in loop mode (default 6)")
    p.add_argument("--round-sleep", type=float, default=120.0, help="seconds between rounds (default 120)")
    p.add_argument("--dry-run", action="store_true", help="read only: report what would happen")
    p.add_argument("--timeout", type=float, default=40.0, help="minutes allowed per pull request (default 40)")
    p.add_argument("--poll", type=float, default=30.0, help="seconds between check polls (default 30)")
    p.add_argument("--settle", type=float, default=20.0, help="seconds to let GitHub register a new run (default 20)")
    p.add_argument("--rerun-grace", type=float, default=180.0, help="seconds a re-run may take to register (default 180)")
    p.add_argument("--also-require", action="append", metavar="CHECK", help="check that must pass in addition to the "
                   "required file (repeatable; default risk-gate)")
    p.add_argument("--base", default=DEFAULT_BASE, help="base branch (default main)")
    p.add_argument("--hold-label", default=DEFAULT_HOLD_LABEL, help="label that keeps a pull request out (default hold)")
    p.add_argument("--allow-unreviewed", action="store_true", help="merge pull requests without the reviewed label "
                   "(default: refuse them unless they are dependency updates)")
    p.add_argument("--include-bots", action="store_true", help="also queue Dependabot pull requests when discovering")
    p.add_argument("--repo", help="OWNER/NAME (default: the current checkout)")
    p.add_argument("--required-file", default=str(DEFAULT_REQUIRED_FILE), help="branch protection JSON listing the required checks")
    p.add_argument("--status-file", help="JSON status file (default $TMPDIR/lattice-merge-queue/status.json)")
    p.add_argument("--log-file", help="human log (default $TMPDIR/lattice-merge-queue/queue.log)")
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        with open(args.required_file, encoding="utf-8") as fh:
            required = required_contexts(json.load(fh))
    except (OSError, ValueError):
        required = None
    if required is None:
        print(f"error: no required checks in {args.required_file}; refusing to run without them", file=sys.stderr)
        return 2
    status_default, log_default = default_paths()
    status_file = Path(args.status_file) if args.status_file else status_default
    log_file = Path(args.log_file) if args.log_file else log_default
    try:
        if not args.status_file or not args.log_file:
            private_dir(status_default.parent)
        for path in (status_file, log_file):
            path.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    opts = Options(base=args.base, hold_label=args.hold_label,
                   reviewed_label="" if args.allow_unreviewed else DEFAULT_REVIEWED_LABEL, include_bots=args.include_bots,
                   also_require=tuple(args.also_require) if args.also_require else DEFAULT_ALSO_REQUIRE,
                   dry_run=args.dry_run, per_pr_timeout=args.timeout * 60, poll=args.poll,
                   settle=args.settle, rerun_grace=args.rerun_grace, rounds=max(1, args.rounds),
                   round_sleep=args.round_sleep, status_file=str(status_file), log_file=str(log_file))
    queue = Queue(GhApi(args.repo), required, opts)
    try:
        left = queue.run(explicit_order(args.prs), args.once)
    except KeyboardInterrupt:
        queue.state["finished"] = True
        queue.state["interrupted"] = True
        queue.write_status()
        queue.log("interrupted")
        return 130
    return 1 if left and not args.dry_run else 0


if __name__ == "__main__":
    sys.exit(main())
