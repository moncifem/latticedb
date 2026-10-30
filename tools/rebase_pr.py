#!/usr/bin/env python3
"""Rebase pull request branches onto the base branch without touching your checkout (issue 350).

With strict up-to-date merging, every merge makes the other open branches stale. For each pull request
this tool, in a private worktree under $TMPDIR:

  1. fetches the branch and the base and rebases the branch onto the base;
  2. on a conflict, resolves ONLY prose list files (docs/gates/*.md, README.md, fuzz/README.md) and
     ONLY hunks where both sides purely added lines (the diff3 base section is empty), by keeping both
     sides (base branch lines first, then the pull request's lines); a hunk where either side edits an
     existing line, or a result that repeats a table row or list item, is refused. A conflict in any other file
     (code, workflows, lock files, CHANGELOG.md, formats) aborts the rebase and is reported, nothing is
     pushed. Generated api-snapshots/*.txt conflicts are allowed only when tools/check_public_api.sh
     exists, because the snapshots are regenerated afterwards;
  3. when Rust files changed: runs `cargo fmt --all -- --check` and, if tools/check_public_api.sh
     exists, regenerates the snapshots of the affected crates and commits them;
  4. pushes with `--force-with-lease` pinned to the branch tip it fetched (a push by someone else in
     the meantime makes it fail instead of overwriting). It never uses a plain `--force`.

Pure functions (file classification, marker resolution, porcelain parsing) have no I/O and are unit
tested in test_rebase_pr.py with synthetic repositories. Usage: docs/merging.md.
"""

import argparse
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

PROSE_PATTERNS = ("docs/gates/*.md", "README.md", "fuzz/README.md")
GENERATED_PATTERNS = ("api-snapshots/*.txt",)
API_SCRIPT = "tools/check_public_api.sh"
SNAPSHOT_COMMIT_MESSAGE = "chore: refresh the public API snapshots after a rebase\n"
# Porcelain codes of a file both sides changed (or both added); anything else (delete vs modify) is refused.
RESOLVABLE_CODES = ("UU", "AA")
MAX_REBASE_STEPS = 500
CRATE_RE = re.compile(r"\Acrates/([^/]+)/")


# ---------------------------------------------------------------------------------------------
# Pure functions
# ---------------------------------------------------------------------------------------------

def matches(path, patterns):
    """Glob match where `*` stays inside one path segment (fnmatch lets it cross a slash)."""
    return any(re.fullmatch("[^/]*".join(re.escape(part) for part in pattern.split("*")), path)
               for pattern in patterns)


def is_prose(path):
    return matches(path, PROSE_PATTERNS)


def is_generated(path):
    return matches(path, GENERATED_PATTERNS)


def parse_unmerged(porcelain):
    """(code, path) for each unmerged entry of `git status --porcelain`."""
    entries = []
    for line in porcelain.splitlines():
        code, path = line[:2], line[3:]
        if code in ("DD", "AU", "UD", "UA", "DU", "AA", "UU"):
            entries.append((code, path))
    return entries


def classify_conflicts(entries, has_api_script):
    """Split unmerged entries into (prose, generated, refused)."""
    prose, generated, refused = [], [], []
    for code, path in entries:
        if is_prose(path) and code in RESOLVABLE_CODES:
            prose.append(path)
        elif is_generated(path) and has_api_script and code in RESOLVABLE_CODES:
            generated.append(path)
        else:
            refused.append(path)
    return prose, generated, refused


ITEM_RE = re.compile(r"\s*(?:\|.*\||[-*+]\s|\d+[.)]\s)")
SEPARATOR_RE = re.compile(r"\s*\|[\s:|-]+\|\s*\Z")


def is_item_line(line):
    """A Markdown table row (not its separator) or a list item."""
    line = line.rstrip("\r\n")
    return bool(ITEM_RE.match(line)) and not SEPARATOR_RE.match(line)


def resolve_additions(text):
    """Keep both sides of every conflict block, base side first, but only for pure additions.

    A block is resolved only when the diff3 base section is empty: both sides inserted lines at the
    same spot and neither edited or deleted an existing line. A block where an existing line was
    edited or removed on either side, a block without a base section, malformed markers, or a table
    row or list item that ends up twice in the file raises ValueError (the caller refuses the file).
    A side repeated verbatim on both sides is kept once. Line endings are preserved.
    """
    out, ours, base, theirs, hunk = [], [], [], [], []
    state = "text"
    has_base = False
    for line in text.splitlines(keepends=True):
        body = line.rstrip("\r\n")
        if state == "text":
            if line.startswith("<<<<<<< "):
                state, ours, base, theirs, has_base = "ours", [], [], [], False
            else:
                out.append(line)
        elif state == "ours":
            if line.startswith("|||||||"):
                state, has_base = "base", True
            elif body == "=======":
                state = "theirs"
            elif line.startswith(("<<<<<<< ", ">>>>>>> ")):
                raise ValueError("malformed conflict markers")
            else:
                ours.append(line)
        elif state == "base":
            if body == "=======":
                state = "theirs"
            elif line.startswith(("<<<<<<< ", ">>>>>>> ")):
                raise ValueError("malformed conflict markers")
            else:
                base.append(line)
        else:
            if line.startswith(">>>>>>> "):
                if not has_base:
                    raise ValueError("conflict block without a base section")
                if base:
                    raise ValueError("both sides edit existing lines")
                out.extend(ours)
                hunk.extend(ours)
                if theirs != ours:
                    if ours and not ours[-1].endswith("\n"):
                        out[-1] += "\n"
                    out.extend(theirs)
                    hunk.extend(theirs)
                state = "text"
            elif line.startswith(("<<<<<<< ", "|||||||")) or body == "=======":
                raise ValueError("malformed conflict markers")
            else:
                theirs.append(line)
    if state != "text":
        raise ValueError("unterminated conflict block")
    merged = "".join(out)
    counts = {}
    for line in merged.splitlines():
        if is_item_line(line):
            counts[line.strip()] = counts.get(line.strip(), 0) + 1
    twice = sorted({h.strip() for h in hunk if is_item_line(h) and counts.get(h.strip(), 0) > 1})
    if twice:
        raise ValueError("a table row or list item would appear twice: " + "; ".join(twice)[:200])
    return merged


def has_markers(text):
    return any(line.startswith(("<<<<<<< ", ">>>>>>> ")) for line in text.splitlines())


def rust_changed(files):
    return any(f.endswith(".rs") for f in files)


def affected_crates(files, snapshot_crates):
    """Crates with changed files that have an api snapshot, sorted."""
    names = set()
    for f in files:
        match = CRATE_RE.match(f)
        if match and match.group(1) in snapshot_crates:
            names.add(match.group(1))
    return sorted(names)


# ---------------------------------------------------------------------------------------------
# git and gh access
# ---------------------------------------------------------------------------------------------

class Failure(Exception):
    """A step that stops this pull request; the message is the report."""


class Refusal(Failure):
    """The tool declines to resolve something; the branch is left as it was."""


def run(cmd, cwd, env=None, check=True):
    full_env = dict(os.environ)
    full_env["GIT_EDITOR"] = "true"
    full_env["GIT_TERMINAL_PROMPT"] = "0"
    if env:
        full_env.update(env)
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=full_env)
    except OSError as exc:
        if check:
            raise Failure(f"{cmd[0]}: {exc}")
        return subprocess.CompletedProcess(cmd, 127, "", str(exc))
    if check and r.returncode:
        raise Failure(f"{' '.join(cmd[:3])}: {(r.stderr or r.stdout).strip()[:300]}")
    return r


class Repo:
    def __init__(self, directory, remote="origin", base="main", gh_repo=None, push=True, keep=False):
        self.dir = directory
        self.remote = remote
        self.base = base
        self.gh_repo = gh_repo
        self.push = push
        self.keep = keep
        self.root = Path(os.environ.get("TMPDIR") or tempfile.gettempdir()) / "lattice-rebase-pr"

    def private_root(self):
        """Create the worktree root (mode 0700) or accept an existing one that is ours and not a link."""
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        st = os.lstat(self.root)
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
            raise Failure(f"{self.root} is not a directory owned by you (symlink or foreign owner)")
        if st.st_mode & 0o077:
            os.chmod(self.root, 0o700)

    def git(self, *args, cwd=None, check=True, env=None):
        return run(["git", *args], cwd or self.dir, env=env, check=check)

    def pr_info(self, n):
        cmd = ["gh", "pr", "view", str(n), "--json", "state,headRefName,baseRefName,isCrossRepository"]
        if self.gh_repo:
            cmd += ["-R", self.gh_repo]
        r = run(cmd, self.dir, check=False)
        try:
            info = json.loads(r.stdout) if r.returncode == 0 else None
        except ValueError:
            info = None
        if not isinstance(info, dict) or not isinstance(info.get("headRefName"), str) \
                or not isinstance(info.get("state"), str):
            raise Failure("cannot read the pull request (API error or malformed answer); nothing changed")
        return info


def rebase_in_progress(repo, wt):
    for name in ("rebase-merge", "rebase-apply"):
        path = repo.git("rev-parse", "--git-path", name, cwd=wt).stdout.strip()
        if os.path.isdir(path if os.path.isabs(path) else os.path.join(wt, path)):
            return True
    return False


def resolve_conflicts(repo, wt, report):
    """Resolve what is allowed; raise Failure listing the rest. Return nothing."""
    entries = parse_unmerged(repo.git("status", "--porcelain", cwd=wt).stdout)
    has_script = os.path.exists(os.path.join(wt, API_SCRIPT))
    prose, generated, refused = classify_conflicts(entries, has_script)
    if refused:
        raise Refusal("conflict in files this tool does not resolve: " + ", ".join(sorted(refused)))
    for path in prose:
        full = os.path.join(wt, path)
        with open(full, encoding="utf-8", newline="") as fh:
            original = fh.read()
        try:
            merged = resolve_additions(original)
        except ValueError as exc:
            raise Refusal(f"conflict in {path} is not a pure addition on both sides ({exc}); "
                          "resolve it by hand")
        if has_markers(merged):
            raise Refusal(f"conflict markers remain in {path}; resolve it by hand")
        with open(full, "w", encoding="utf-8", newline="") as fh:
            fh.write(merged)
        repo.git("add", "--", path, cwd=wt)
        report["resolved"].append(path)
    for path in generated:
        # Rebasing: "ours" is the base branch. The snapshots are regenerated after the rebase.
        repo.git("checkout", "--ours", "--", path, cwd=wt)
        repo.git("add", "--", path, cwd=wt)
        report["resolved"].append(path + " (regenerated)")


def skip_empty_commit(repo, wt, report):
    """`git rebase --skip`, but only for a commit that really became empty."""
    staged = repo.git("diff", "--cached", "--quiet", cwd=wt, check=False).returncode
    unstaged = repo.git("diff", "--quiet", cwd=wt, check=False).returncode
    if staged or unstaged:
        raise Failure("git rebase stopped without conflicts but the commit still has changes (a commit hook "
                      "failed?); not skipping it, nothing pushed")
    label = repo.git("log", "-1", "--format=%h %s", "REBASE_HEAD", cwd=wt, check=False).stdout.strip()
    report["skipped"].append(label or "an unidentified commit")
    return repo.git("rebase", "--skip", cwd=wt, check=False)


def rebase(repo, wt, report):
    # diff3 markers carry the base section, which resolve_additions needs to prove "pure addition".
    style = ["-c", "merge.conflictStyle=diff3"]
    r = repo.git(*style, "rebase", f"{repo.remote}/{repo.base}", cwd=wt, check=False)
    steps = 0
    while r.returncode != 0:
        steps += 1
        if steps > MAX_REBASE_STEPS or not rebase_in_progress(repo, wt):
            raise Failure(f"git rebase failed: {(r.stderr or r.stdout).strip()[:300]}")
        unmerged = parse_unmerged(repo.git("status", "--porcelain", cwd=wt).stdout)
        if unmerged:
            resolve_conflicts(repo, wt, report)
            r = repo.git(*style, "rebase", "--continue", cwd=wt, check=False)
        else:
            r = skip_empty_commit(repo, wt, report)


def check_rust(repo, wt, changed, report):
    if not rust_changed(changed):
        report["notes"].append("no Rust files changed: cargo fmt and snapshots skipped")
        return
    fmt = run(["cargo", "fmt", "--all", "--", "--check"], wt, check=False)
    if fmt.returncode != 0:
        raise Failure("cargo fmt --all -- --check fails after the rebase; fix the formatting on the branch "
                      "(nothing pushed): " + (fmt.stdout or fmt.stderr).strip()[:200])
    report["notes"].append("cargo fmt --check passed")
    script = os.path.join(wt, API_SCRIPT)
    if not os.path.exists(script):
        return
    snaps = os.path.join(wt, "api-snapshots")
    have = {p[:-4] for p in os.listdir(snaps) if p.endswith(".txt")} if os.path.isdir(snaps) else set()
    crates = affected_crates(changed, have)
    if not crates:
        report["notes"].append("no crate with an api snapshot changed: snapshots not regenerated")
        return
    upd = run(["bash", script, "--update", *crates], wt, check=False)
    if upd.returncode != 0:
        raise Failure(f"{API_SCRIPT} --update failed (nothing pushed): " + (upd.stderr or upd.stdout).strip()[-300:])
    if repo.git("status", "--porcelain", "--", "api-snapshots", cwd=wt).stdout.strip():
        repo.git("add", "--", "api-snapshots", cwd=wt)
        fd, msg = tempfile.mkstemp(dir=str(repo.root), prefix="snapshot-msg-")
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(SNAPSHOT_COMMIT_MESSAGE)
        try:
            repo.git("commit", "-q", "-F", msg, "--", "api-snapshots", cwd=wt)
        finally:
            os.unlink(msg)
        report["notes"].append("regenerated and committed api snapshots: " + ", ".join(crates))
    else:
        report["notes"].append("api snapshots unchanged: " + ", ".join(crates))


def process(repo, n):
    """Rebase one pull request. Returns a report dict; status is one of
    rebased, up-to-date, refused, failed, skipped."""
    report = {"pr": n, "status": "failed", "resolved": [], "skipped": [], "notes": [], "message": ""}
    wt = None
    try:
        info = repo.pr_info(n)
        if info["state"] != "OPEN":
            report.update(status="skipped", message=f"pull request is {info['state'].lower()}")
            return report
        if info.get("isCrossRepository") is not False:
            report.update(status="skipped", message="head is in a fork or the repository is unknown; "
                                                     "this tool cannot push there")
            return report
        if info.get("baseRefName", repo.base) != repo.base:
            report.update(status="skipped", message=f"base is {info['baseRefName']}, not {repo.base}")
            return report
        branch = info["headRefName"]
        repo.git("fetch", "-q", repo.remote,
                 f"+refs/heads/{repo.base}:refs/remotes/{repo.remote}/{repo.base}",
                 f"+refs/heads/{branch}:refs/remotes/{repo.remote}/{branch}")
        old_head = repo.git("rev-parse", f"{repo.remote}/{branch}").stdout.strip()
        if repo.git("merge-base", "--is-ancestor", f"{repo.remote}/{repo.base}", old_head, check=False).returncode == 0:
            report.update(status="up-to-date", message="already contains the base branch; nothing to do")
            return report
        repo.private_root()
        wt = tempfile.mkdtemp(prefix=f"pr-{n}-", dir=str(repo.root))
        repo.git("worktree", "add", "-q", "--detach", wt, old_head)
        rebase(repo, wt, report)
        base_ref = f"{repo.remote}/{repo.base}"
        if repo.git("rev-parse", "HEAD", cwd=wt).stdout.strip() == repo.git("rev-parse", base_ref).stdout.strip():
            raise Failure("after the rebase the branch is identical to the base branch (every commit was "
                          "dropped); nothing pushed")
        changed = repo.git("diff", "--name-only", f"{base_ref}...HEAD", cwd=wt).stdout.split()
        check_rust(repo, wt, changed, report)
        new_head = repo.git("rev-parse", "HEAD", cwd=wt).stdout.strip()
        if not repo.push:
            report.update(status="rebased", message=f"rebased locally to {new_head[:10]}; push disabled")
            return report
        repo.git("push", "-q", f"--force-with-lease=refs/heads/{branch}:{old_head}", repo.remote,
                 f"{new_head}:refs/heads/{branch}", cwd=wt)
        report.update(status="rebased", message=f"pushed {new_head[:10]} (was {old_head[:10]})")
    except Failure as exc:
        text = str(exc)
        report.update(status="refused" if isinstance(exc, Refusal) else "failed", message=text)
    finally:
        if wt:
            try:
                if rebase_in_progress(repo, wt):
                    repo.git("rebase", "--abort", cwd=wt, check=False)
                if not repo.keep:
                    repo.git("worktree", "remove", "--force", wt, check=False)
                    shutil.rmtree(wt, ignore_errors=True)
            except Exception as exc:  # cleanup must not hide the real result
                report["notes"].append(f"cleanup failed, remove {wt} by hand: {exc}")
    return report


def format_report(report):
    line = f"PR {report['pr']}: {report['status'].upper()}: {report['message']}".rstrip(": ")
    extra = [f"  resolved (kept both sides): {p}" for p in report["resolved"]]
    extra += [f"  skipped (became empty after the rebase): {c}" for c in report["skipped"]]
    extra += [f"  {note}" for note in report["notes"]]
    return "\n".join([line, *extra])


def build_parser():
    p = argparse.ArgumentParser(description="Rebase pull request branches onto the base branch in a private worktree.")
    p.add_argument("prs", nargs="+", type=int, metavar="PR")
    p.add_argument("--base", default="main", help="base branch (default main)")
    p.add_argument("--remote", default="origin", help="remote of the repository (default origin)")
    p.add_argument("--repo", help="OWNER/NAME for gh (default: the current checkout)")
    p.add_argument("--repo-dir", default=".", help="a checkout of the repository (default: the current directory)")
    p.add_argument("--no-push", action="store_true", help="rebase and check, but do not push")
    p.add_argument("--keep", action="store_true", help="keep the worktree for inspection")
    p.add_argument("--json", action="store_true", help="print the reports as JSON")
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    top = run(["git", "rev-parse", "--show-toplevel"], args.repo_dir, check=False)
    if top.returncode != 0:
        print("error: not inside a git checkout", file=sys.stderr)
        return 2
    repo = Repo(top.stdout.strip(), args.remote, args.base, args.repo, push=not args.no_push, keep=args.keep)
    reports = [process(repo, n) for n in args.prs]
    if args.json:
        print(json.dumps(reports, indent=2))
    else:
        for report in reports:
            print(format_report(report))
    return 0 if all(r["status"] in ("rebased", "up-to-date", "skipped") for r in reports) else 1


if __name__ == "__main__":
    sys.exit(main())
