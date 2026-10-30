#!/usr/bin/env python3
"""Tests for rebase_pr.py against synthetic repositories: a bare "origin", a seed clone that plays the
pull request authors, and a coordinator clone the tool runs from. `gh` and `cargo` are fake scripts put
first on PATH (the fake cargo records its arguments); nothing here contacts GitHub or builds Rust."""
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import rebase_pr as rp

GATE = "docs/gates/phase-2.md"
README = "README.md"
LIB = "crates/lattice-core/src/lib.rs"

FAKE_GH = r'''#!%(python)s
import json, os, sys
prs = json.load(open(os.environ["FAKE_PRS"]))
n = sys.argv[3]
if n not in prs or prs[n] is None:
    sys.stderr.write("HTTP 502")
    sys.exit(1)
sys.stdout.write(json.dumps(prs[n]))
'''

FAKE_CARGO = r'''#!%(python)s
import os, subprocess, sys
open(os.environ["FAKE_CARGO_LOG"], "a").write(" ".join(sys.argv[1:]) + "\n")
hook = os.environ.get("FAKE_CARGO_HOOK")
if hook:
    subprocess.run([hook], check=True)
sys.exit(int(os.environ.get("FAKE_CARGO_RC", "0")))
'''


def write_exec(path, text):
    Path(path).write_text(text)
    Path(path).chmod(Path(path).stat().st_mode | stat.S_IEXEC)


class RepoCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        write_exec(bin_dir / "gh", FAKE_GH % {"python": sys.executable})
        write_exec(bin_dir / "cargo", FAKE_CARGO % {"python": sys.executable})
        self.prs_file = self.root / "prs.json"
        self.cargo_log = self.root / "cargo.log"
        self.tmpdir = self.root / "tmp"
        self.tmpdir.mkdir()
        env = {"PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}", "FAKE_PRS": str(self.prs_file),
               "FAKE_CARGO_LOG": str(self.cargo_log), "TMPDIR": str(self.tmpdir),
               "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "t@example.invalid",
               "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "t@example.invalid",
               "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
        saved = {k: os.environ.get(k) for k in env}
        os.environ.update(env)
        self.addCleanup(self.restore, saved)
        for key in ("FAKE_CARGO_RC", "FAKE_CARGO_HOOK"):
            os.environ.pop(key, None)
        self.origin = self.root / "origin.git"
        self.git("init", "-q", "--bare", "-b", "main", str(self.origin), cwd=self.root)
        self.seed = self.clone("seed")
        self.commit(self.seed, {GATE: "# Phase 2\n\n| row | state |\n|---|---|\n| a | open |\n", README: "# Lattice\n\n- one\n",
                                LIB: "pub fn f() {}\n", "Cargo.lock": "lock 1\n", "CHANGELOG.md": "# Changelog\n"},
                    "chore: initial")
        self.git("push", "-q", "origin", "HEAD:main", cwd=self.seed)
        self.coord = self.clone("coord")
        self.set_prs({})

    @staticmethod
    def restore(saved):
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        for key in ("FAKE_CARGO_RC", "FAKE_CARGO_HOOK"):
            os.environ.pop(key, None)

    # -- helpers --------------------------------------------------------------------------
    def git(self, *args, cwd):
        r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, f"git {' '.join(args)}: {r.stderr}")
        return r.stdout.strip()

    def clone(self, name):
        path = self.root / name
        self.git("clone", "-q", str(self.origin), str(path), cwd=self.root)
        return path

    def commit(self, repo, files, message):
        for rel, text in files.items():
            full = Path(repo) / rel
            full.parent.mkdir(parents=True, exist_ok=True)
            full.write_text(text)
        self.git("add", "-A", cwd=repo)
        self.git("commit", "-q", "-m", message, cwd=repo)

    def advance_main(self, files, message="docs: change main"):
        self.git("checkout", "-q", "-B", "main", "origin/main", cwd=self.seed)
        self.commit(self.seed, files, message)
        self.git("push", "-q", "origin", "main", cwd=self.seed)

    def make_branch(self, name, files, message="feat: change"):
        self.git("fetch", "-q", "origin", cwd=self.seed)
        self.git("checkout", "-q", "-B", name, "origin/main", cwd=self.seed)
        self.commit(self.seed, files, message)
        self.git("push", "-q", "-f", "origin", name, cwd=self.seed)
        self.git("checkout", "-q", "main", cwd=self.seed)

    def set_prs(self, prs):
        self.prs_file.write_text(json.dumps(prs))

    def open_pr(self, n, branch, **extra):
        data = json.loads(self.prs_file.read_text())
        data[str(n)] = {"state": "OPEN", "headRefName": branch, "baseRefName": "main",
                        "isCrossRepository": False, **extra}
        self.set_prs(data)

    def origin_rev(self, ref):
        return self.git("rev-parse", ref, cwd=self.origin)

    def origin_show(self, ref, path):
        return self.git("show", f"{ref}:{path}", cwd=self.origin)

    def run_tool(self, *prs, push=True):
        repo = rp.Repo(str(self.coord), push=push)
        return [rp.process(repo, n) for n in prs]

    def cargo_calls(self):
        return self.cargo_log.read_text().splitlines() if self.cargo_log.exists() else []

    def leftovers(self):
        root = self.tmpdir / "lattice-rebase-pr"
        return sorted(p.name for p in root.iterdir()) if root.exists() else []

    def assert_contains_main(self, branch):
        r = subprocess.run(["git", "merge-base", "--is-ancestor", "main", branch], cwd=self.origin)
        self.assertEqual(r.returncode, 0, f"{branch} does not contain main")


class RebaseTests(RepoCase):
    def test_clean_rebase_is_pushed_with_a_lease_and_keeps_the_commits(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"}, "docs: add a")
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "rebased", report)
        self.assert_contains_main("feat/a")
        self.assertEqual(self.origin_show("feat/a", "docs/a.md"), "a")
        self.assertEqual(self.git("log", "-1", "--format=%s", "feat/a", cwd=self.origin), "docs: add a")
        self.assertEqual(self.leftovers(), [])
        worktrees = self.git("worktree", "list", cwd=self.coord).splitlines()
        self.assertEqual(len(worktrees), 1)

    def test_up_to_date_branch_is_left_alone(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"})
        self.open_pr(11, "feat/a")
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "up-to-date")
        self.assertEqual(self.origin_rev("feat/a"), before)

    def test_prose_conflicts_keep_both_sides(self):
        self.make_branch("feat/a", {GATE: "# Phase 2\n\n| row | state |\n|---|---|\n| a | open |\n| pr | done |\n",
                                    README: "# Lattice\n\n- one\n- from the pr\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({GATE: "# Phase 2\n\n| row | state |\n|---|---|\n| a | open |\n| main | done |\n",
                           README: "# Lattice\n\n- one\n- from main\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "rebased", report)
        self.assertEqual(sorted(report["resolved"]), sorted([GATE, README]))
        gate = self.origin_show("feat/a", GATE)
        self.assertEqual(gate.splitlines()[-2:], ["| main | done |", "| pr | done |"])
        self.assertEqual(self.origin_show("feat/a", README).splitlines()[-2:], ["- from main", "- from the pr"])
        self.assertNotIn("<<<<<<<", gate)
        self.assertNotIn(">>>>>>>", gate)
        self.assert_contains_main("feat/a")

    def test_conflicts_across_several_commits_are_all_resolved(self):
        self.make_branch("feat/a", {README: "# Lattice\n\n- one\n- pr one\n"}, "docs: first")
        self.git("checkout", "-q", "feat/a", cwd=self.seed)
        self.commit(self.seed, {README: "# Lattice\n\n- one\n- pr one\n- pr two\n"}, "docs: second")
        self.git("push", "-q", "origin", "feat/a", cwd=self.seed)
        self.open_pr(11, "feat/a")
        self.advance_main({README: "# Lattice\n\n- one\n- main one\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "rebased", report)
        text = self.origin_show("feat/a", README)
        for line in ("- main one", "- pr one", "- pr two"):
            self.assertIn(line, text)
        self.assertNotIn("<<<<<<<", text)

    def test_code_conflict_is_refused_and_nothing_is_pushed(self):
        self.make_branch("feat/a", {LIB: "pub fn f() { 1 }\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({LIB: "pub fn f() { 2 }\n"})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "refused")
        self.assertIn(LIB, report["message"])
        self.assertEqual(self.origin_rev("feat/a"), before)
        self.assertEqual(self.leftovers(), [])
        self.assertEqual(self.cargo_calls(), [])

    def test_workflow_lock_and_changelog_conflicts_are_refused(self):
        wf = ".github/workflows/x.yml"
        self.advance_main({wf: "on: push\n"})
        self.make_branch("feat/a", {wf: "on: pull_request\n"})
        self.make_branch("feat/b", {"Cargo.lock": "lock pr\n"})
        self.make_branch("feat/c", {"CHANGELOG.md": "# Changelog\n- pr\n"})
        self.open_pr(11, "feat/a")
        self.open_pr(12, "feat/b")
        self.open_pr(13, "feat/c")
        self.advance_main({wf: "on: [push, merge_group]\n", "Cargo.lock": "lock main\n",
                           "CHANGELOG.md": "# Changelog\n- main\n"})
        reports = self.run_tool(11, 12, 13)
        self.assertEqual([r["status"] for r in reports], ["refused"] * 3)
        for report, path in zip(reports, (wf, "Cargo.lock", "CHANGELOG.md")):
            self.assertIn(path, report["message"])

    def test_mixed_prose_and_code_conflict_pushes_nothing_and_resolves_nothing(self):
        self.make_branch("feat/a", {README: "# Lattice\n\n- one\n- pr\n", LIB: "pub fn f() { 1 }\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({README: "# Lattice\n\n- one\n- main\n", LIB: "pub fn f() { 2 }\n"})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "refused")
        self.assertEqual(report["resolved"], [])
        self.assertNotIn(README, report["message"])
        self.assertEqual(self.origin_rev("feat/a"), before)

    def test_no_push_mode_rebases_but_leaves_origin_alone(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11, push=False)
        self.assertEqual(report["status"], "rebased")
        self.assertEqual(self.origin_rev("feat/a"), before)


class RustChecksTests(RepoCase):
    def test_docs_only_change_does_not_run_cargo(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        self.assertEqual(self.run_tool(11)[0]["status"], "rebased")
        self.assertEqual(self.cargo_calls(), [])

    def test_rust_change_runs_fmt_check(self):
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        self.assertEqual(self.run_tool(11)[0]["status"], "rebased")
        self.assertEqual(self.cargo_calls(), ["fmt --all -- --check"])

    def test_failed_fmt_check_blocks_the_push(self):
        os.environ["FAKE_CARGO_RC"] = "1"
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g(){}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "failed")
        self.assertIn("cargo fmt", report["message"])
        self.assertEqual(self.origin_rev("feat/a"), before)
        self.assertEqual(self.leftovers(), [])

    def add_api_script(self, body):
        script = "#!/usr/bin/env bash\n" + body
        self.git("checkout", "-q", "main", cwd=self.seed)
        self.advance_main({"tools/check_public_api.sh": script, "api-snapshots/lattice-core.txt": "api v1\n"})

    def test_api_snapshots_are_regenerated_and_committed_when_the_script_exists(self):
        self.add_api_script('echo "$@" > "$(dirname "$0")/../.args"\n'
                            'for c in "${@:2}"; do echo "api regenerated $c" > "$(dirname "$0")/../api-snapshots/$c.txt"; done\n')
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "rebased", report)
        self.assertEqual(self.origin_show("feat/a", "api-snapshots/lattice-core.txt"), "api regenerated lattice-core")
        self.assertEqual(self.git("log", "-1", "--format=%s", "feat/a", cwd=self.origin),
                         "chore: refresh the public API snapshots after a rebase")
        self.assertNotIn("api-snapshots/lattice-sim.txt",
                         self.git("ls-tree", "-r", "--name-only", "feat/a", cwd=self.origin))

    def test_api_snapshots_are_not_touched_for_docs_only_or_without_the_script(self):
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        (report,) = self.run_tool(11)  # no script in the tree
        self.assertEqual(report["status"], "rebased")
        self.assertEqual(self.git("log", "--format=%s", "main..feat/a", cwd=self.origin), "feat: change")

    def test_api_update_failure_blocks_the_push(self):
        self.add_api_script("echo broken >&2\nexit 2\n")
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "failed")
        self.assertIn("check_public_api.sh", report["message"])
        self.assertEqual(self.origin_rev("feat/a"), before)

    def test_generated_snapshot_conflict_is_allowed_only_with_the_script(self):
        snap = "api-snapshots/lattice-core.txt"
        self.add_api_script('for c in "${@:2}"; do echo "api regenerated $c" > "$(dirname "$0")/../api-snapshots/$c.txt"; done\n')
        self.make_branch("feat/a", {snap: "api pr\n", "crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({snap: "api main\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "rebased", report)
        self.assertEqual(self.origin_show("feat/a", snap), "api regenerated lattice-core")

    def test_generated_snapshot_conflict_is_refused_without_the_script(self):
        snap = "api-snapshots/lattice-core.txt"
        self.advance_main({snap: "api v1\n"})
        self.make_branch("feat/a", {snap: "api pr\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({snap: "api main\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "refused")


class SafetyTests(RepoCase):
    def test_a_push_by_someone_else_during_the_run_is_not_overwritten(self):
        hook = self.root / "hook.sh"
        write_exec(hook, "#!/bin/sh\ncd '%s'\ngit fetch -q origin\ngit checkout -q -B feat/a origin/feat/a\n"
                         "echo late > late.txt\ngit add late.txt\ngit commit -q -m 'docs: late push'\n"
                         "git push -q origin feat/a\n" % self.seed)
        os.environ["FAKE_CARGO_HOOK"] = str(hook)
        self.make_branch("feat/a", {"crates/lattice-core/src/new.rs": "pub fn g() {}\n"})
        self.open_pr(11, "feat/a")
        self.advance_main({"docs/other.md": "x\n"})
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "failed", report)
        self.assertEqual(self.git("log", "-1", "--format=%s", "feat/a", cwd=self.origin), "docs: late push")

    def test_api_error_is_unknown_and_changes_nothing(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"})
        self.set_prs({"11": None})
        before = self.origin_rev("feat/a")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "failed")
        self.assertIn("cannot read", report["message"])
        self.assertEqual(self.origin_rev("feat/a"), before)

    def test_closed_fork_and_foreign_base_pull_requests_are_skipped(self):
        self.make_branch("feat/a", {"docs/a.md": "a\n"})
        self.open_pr(11, "feat/a", state="MERGED")
        self.open_pr(12, "feat/a", isCrossRepository=True)
        self.open_pr(13, "feat/a", baseRefName="release")
        self.advance_main({"docs/other.md": "x\n"})
        before = self.origin_rev("feat/a")
        reports = self.run_tool(11, 12, 13)
        self.assertEqual([r["status"] for r in reports], ["skipped"] * 3)
        self.assertEqual(self.origin_rev("feat/a"), before)

    def test_missing_remote_branch_fails_cleanly(self):
        self.open_pr(11, "feat/gone")
        (report,) = self.run_tool(11)
        self.assertEqual(report["status"], "failed")

    def test_tool_source_has_no_plain_force_push(self):
        text = Path(rp.__file__).read_text()
        pushes = [line for line in text.splitlines() if '"push"' in line]
        self.assertEqual(len(pushes), 1)
        self.assertIn("--force-with-lease=", pushes[0])
        self.assertNotIn('"-f"', text)
        self.assertNotIn('"--force", repo.remote', text)


class PureTests(unittest.TestCase):
    def test_prose_and_generated_classification(self):
        for path in ("docs/gates/phase-1.md", "docs/gates/phase-2.md", "README.md", "fuzz/README.md"):
            self.assertTrue(rp.is_prose(path), path)
        for path in ("CHANGELOG.md", "crates/x/README.md", "docs/gates/sub/x.md", "docs/gates/x.rs",
                     "docs/roadmap.md", ".github/workflows/pr.yml", "Cargo.lock", "tools/README.md"):
            self.assertFalse(rp.is_prose(path), path)
        self.assertTrue(rp.is_generated("api-snapshots/lattice-core.txt"))
        self.assertFalse(rp.is_generated("api-snapshots/x/y.md"))

    def test_classify_conflicts(self):
        entries = [("UU", "docs/gates/phase-2.md"), ("UU", "src/a.rs"), ("DU", "README.md"),
                   ("UU", "api-snapshots/lattice-abi.txt"), ("AA", "fuzz/README.md")]
        self.assertEqual(rp.classify_conflicts(entries, False),
                         (["docs/gates/phase-2.md", "fuzz/README.md"], [],
                          ["src/a.rs", "README.md", "api-snapshots/lattice-abi.txt"]))
        self.assertEqual(rp.classify_conflicts(entries, True)[1], ["api-snapshots/lattice-abi.txt"])

    def test_parse_unmerged(self):
        porcelain = "UU a.md\n M b.md\nAA c.md\nDU d.md\n?? e\nA  f\n"
        self.assertEqual(rp.parse_unmerged(porcelain), [("UU", "a.md"), ("AA", "c.md"), ("DU", "d.md")])

    def test_resolve_union(self):
        text = "head\n<<<<<<< a\nmain\n=======\npr\n>>>>>>> b\ntail\n"
        self.assertEqual(rp.resolve_union(text), "head\nmain\npr\ntail\n")
        diff3 = "<<<<<<< a\nmain\n||||||| base\nold\n=======\npr\n>>>>>>> b\n"
        self.assertEqual(rp.resolve_union(diff3), "main\npr\n")
        same = "<<<<<<< a\nsame\n=======\nsame\n>>>>>>> b\n"
        self.assertEqual(rp.resolve_union(same), "same\n")
        two = "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n"
        self.assertEqual(rp.resolve_union(two), "1\n2\nmid\n3\n4\n")
        self.assertEqual(rp.resolve_union("Title\n=======\nbody\n"), "Title\n=======\nbody\n")
        self.assertEqual(rp.resolve_union("<<<<<<< a\nmain\n=======\nno newline at end\n>>>>>>> b\n"),
                         "main\nno newline at end\n")
        self.assertEqual(rp.resolve_union("<<<<<<< a\nx\n=======\ny\n"), None)
        self.assertEqual(rp.resolve_union("<<<<<<< a\n<<<<<<< b\n=======\ny\n>>>>>>> c\n"), None)
        self.assertEqual(rp.resolve_union("<<<<<<< a\nx\ny\n>>>>>>> b\n"), None)

    def test_rust_detection_and_affected_crates(self):
        self.assertTrue(rp.rust_changed(["docs/a.md", "crates/x/src/lib.rs"]))
        self.assertFalse(rp.rust_changed(["Cargo.toml", "docs/a.md"]))
        files = ["crates/lattice-core/src/a.rs", "crates/lattice-sys/src/b.rs", "docs/x.md", "crates/lattice-abi/Cargo.toml"]
        self.assertEqual(rp.affected_crates(files, {"lattice-core", "lattice-abi"}), ["lattice-abi", "lattice-core"])
        self.assertEqual(rp.affected_crates(files, set()), [])


if __name__ == "__main__":
    unittest.main()
