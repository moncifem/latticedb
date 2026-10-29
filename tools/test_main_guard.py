"""Pure decisions plus actual Git/bare-origin and fake-gh rehearsal. No live GitHub mutation."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "guard", Path(__file__).with_name("main_guard.py")
)
g = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(g)

FAKE_GH = r"""#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
from urllib.parse import parse_qs
p=Path(os.environ['MAIN_GUARD_FIXTURE_STATE']); s=json.loads(p.read_text()); a=sys.argv[1:]
assert a[0]=='api',a
if '--method' in a:
 i=a.index('--method'); method=a[i+1]; path=a[i+2]
else: method='GET';path=a[1]
fields=json.load(sys.stdin) if '--input' in a else None
s['calls'].append({'method':method,'path':path,'fields':fields})
route=path.split('?',1)[0]; query=parse_qs(path.split('?',1)[1] if '?' in path else '')
prefix='repos/moncifem/latticedb'
def save():p.write_text(json.dumps(s))
def reply(v):save();print(json.dumps(v));sys.exit(0)
def advance():
 subprocess.run(['git','--git-dir',s['origin'],'update-ref','refs/heads/main',s['future']],check=True,capture_output=True)
if method!='GET':
 assert route.startswith(prefix+'/'),route
 s['writes'].append({'path':route,'fields':fields})
 if route==prefix+'/labels':s['labels'].append(fields);reply(fields)
 if route==prefix+'/issues':
  issue={'number':7,'state':'open',**fields};s['issues'].append(issue)
  if s.get('advance_after_issue'):advance()
  reply(issue)
 if route==prefix+'/issues/8/labels':reply(fields)
 if route.endswith('/comments'):
  s.setdefault('comments',[]).append(fields);reply(fields)
 if route==prefix+'/pulls':
  assert fields['draft'] is True,fields
  assert fields['base']=='main' and fields['head']=='revert/'+s['run']['head_sha']
  s['prs'].append({'number':8,'head':{'ref':fields['head'],'repo':{'full_name':'moncifem/latticedb'}},'base':{'ref':'main'},**fields})
  reply({'number':8,'html_url':'https://github.com/moncifem/latticedb/pull/8'})
 raise AssertionError((method,path,fields))
if route==prefix+'/actions/workflows/pr.yml':reply(s['workflow'])
if route==prefix+'/actions/runs/200':reply(s['run'])
if route==prefix+'/actions/runs/100':reply(next(r for r in s['runs'] if r['id']==100))
if route==prefix+'/actions/workflows/42/runs':
 if s.get('advance_on_runs'):advance()
 data=s['runs']; page=int(query.get('page',['1'])[0]);reply({'total_count':len(data),'workflow_runs':data[(page-1)*100:page*100]})
if route==prefix+'/actions/runs/200/attempts/1/jobs':
 data=s['jobs'];page=int(query.get('page',['1'])[0]);reply({'total_count':len(data),'jobs':data[(page-1)*100:page*100]})
if route.startswith(prefix+'/actions/jobs/') and route.endswith('/logs'):
 ident=route.split('/')[-2]
 if ident in s.get('deny_logs',[]):save();sys.exit(1)
 save();print(s['logs'][ident]);sys.exit(0)
if route==prefix+'/commits/'+s['run']['head_sha']:reply(s['commit'])
if route==prefix+'/pulls':
 if s.get('race_branch'):
  worktrees=subprocess.check_output(['git','-C',s['work'],'worktree','list','--porcelain'],text=True)
  if worktrees.count('worktree ')>1:
   subprocess.run(['git','--git-dir',s['origin'],'update-ref','refs/heads/revert/'+s['run']['head_sha'],s['good']],check=True,capture_output=True)
 reply(s['prs'])
if route==prefix+'/issues':reply(s['issues'])
if route.endswith('/comments'):reply(s.get('comments',[]))
if route==prefix+'/labels':reply(s['labels'])
raise AssertionError(path)
"""


class Decisions(unittest.TestCase):
    def test_only_explicit_code_and_proven_single_delta_proposes(self):
        self.assertEqual(
            g.decide("failure", {"msrv": "code"}, culprit_known=True).action,
            g.REVERT_PR,
        )
        for condition in ("is_revert", "duplicate", "bot", "manual"):
            self.assertEqual(
                g.decide(
                    "failure", {"msrv": "code"}, culprit_known=True, **{condition: True}
                ).action,
                g.ISSUE_ONLY,
            )
        self.assertEqual(g.decide("failure", {"msrv": "code"}).action, g.ISSUE_ONLY)
        self.assertEqual(
            g.decide("failure", {"msrv": "unknown"}, culprit_known=True).action,
            g.ISSUE_ONLY,
        )

    def test_optional_cancelled_skipped_timeout_infra_never_blame_code(self):
        for state in ("none", "infra"):
            self.assertEqual(
                g.decide(
                    "failure",
                    {"msrv": state, "gate (macos-latest)": "code"},
                    culprit_known=True,
                ).action,
                g.NONE,
            )
        for conclusion in ("success", "cancelled", "timed_out", "skipped", None):
            self.assertEqual(
                g.decide(conclusion, {"msrv": "code"}, culprit_known=True).action,
                g.NONE,
            )

    def test_failure_classification_unknown_is_not_code(self):
        job = {"name": "msrv", "conclusion": "failure", "steps": []}
        self.assertEqual(g.classify_job(job, "error[E0308]: mismatch"), "unknown")
        job["steps"] = [
            {"name": "Run cargo check --workspace", "conclusion": "failure"}
        ]
        self.assertEqual(g.classify_job(job, "failed to download: HTTP 503"), "infra")
        self.assertEqual(
            g.classify_job(job, "error[E0308]: mismatch\nHTTP 503"), "unknown"
        )
        self.assertEqual(g.classify_job(job, "random diagnostic"), "unknown")
        self.assertEqual(g.classify_job(job, "error[E0308]: mismatch"), "code")
        job["steps"] = [
            {"name": "Post dangerous code validator", "conclusion": "failure"}
        ]
        self.assertEqual(g.classify_job(job, "error[E0308]: mismatch"), "unknown")
        job["steps"] = [{"name": "Run actions/checkout@pin", "conclusion": "failure"}]
        self.assertEqual(g.classify_job(job, "network failed"), "unknown")
        for conclusion in ("success", "cancelled", "timed_out", "skipped", "neutral"):
            job["conclusion"] = conclusion
            self.assertEqual(g.classify_job(job, None), "none")

    def test_revert_detection_is_anchored(self):
        for subject in ('Revert "feat: x"', "revert: x", "revert(core): x"):
            self.assertTrue(g.is_revert_subject(subject))
        self.assertFalse(g.is_revert_subject("feat: revert support"))


class Rehearsal(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="main-guard-fixture-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.work = self.root / "checkout"
        self.origin = self.root / "origin.git"
        self.work.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.call(["git", "init", "--bare", str(self.origin)])
        self.call(["git", "init", "-b", "main", str(self.work)])
        self.git("config", "user.name", "Fixture Owner")
        self.git("config", "user.email", "fixture-owner@example.invalid")
        self.git("remote", "add", "origin", "https://github.com/moncifem/latticedb.git")
        self.git(
            "config",
            f"url.{self.origin.as_uri()}.insteadOf",
            "https://github.com/moncifem/latticedb.git",
        )
        (self.work / "tools").mkdir()
        (self.work / "tools/branch-protection.json").write_text(
            json.dumps(
                {"required_status_checks": {"contexts": sorted(g.REQUIRED_CHECKS)}}
            )
        )
        self.source = "crates/lattice-api/src/case.rs"
        (self.work / self.source).parent.mkdir(parents=True)
        (self.work / self.source).write_text("good\n")
        self.git("add", ".")
        self.git("commit", "-m", "test: green fixture")
        self.good = self.git("rev-parse", "HEAD").strip()
        (self.work / self.source).write_text("bad\n")
        self.git("commit", "-am", "test: red fixture")
        self.bad = self.git("rev-parse", "HEAD").strip()
        self.git("push", "origin", "main")
        (self.work / self.source).write_text("future fix\n")
        self.git("commit", "-am", "test: fixture future fix")
        self.future = self.git("rev-parse", "HEAD").strip()
        self.git("push", "origin", "HEAD:refs/heads/fixture-future")
        self.git("reset", "--hard", self.bad)

        def run(ident, sha, date, conclusion):
            return {
                "id": ident,
                "workflow_id": 42,
                "path": g.WORKFLOW_PATH,
                "repository": {"full_name": g.REPOSITORY},
                "head_repository": {"full_name": g.REPOSITORY},
                "event": "push",
                "head_branch": "main",
                "status": "completed",
                "head_sha": sha,
                "run_attempt": 1,
                "conclusion": conclusion,
                "created_at": date,
            }

        current = run(200, self.bad, "2026-10-29T01:00:00Z", "failure")
        previous = run(100, self.good, "2026-10-29T00:00:00Z", "success")
        jobs = []
        for at, name in enumerate(sorted(g.REQUIRED_CHECKS)):
            failed = name == "msrv"
            jobs.append(
                {
                    "id": at + 1,
                    "name": name,
                    "conclusion": "failure" if failed else "success",
                    "steps": [
                        {"name": "Run cargo check --workspace", "conclusion": "failure"}
                    ]
                    if failed
                    else [],
                }
            )
        failed = str(next(j["id"] for j in jobs if j["name"] == "msrv"))
        self.state = {
            "origin": str(self.origin),
            "work": str(self.work),
            "good": self.good,
            "future": self.future,
            "workflow": {"id": 42, "path": g.WORKFLOW_PATH},
            "run": current,
            "runs": [current, previous],
            "jobs": jobs,
            "logs": {
                failed: "error[E0308]: mismatched types\n --> "
                + self.source
                + ":1:2\nsecret-not-for-output"
            },
            "commit": {
                "sha": self.bad,
                "author": {"login": "moncifem"},
                "committer": {"login": "moncifem"},
            },
            "prs": [],
            "issues": [],
            "labels": [{"name": "main-red"}],
            "calls": [],
            "writes": [],
        }
        self.state_path = self.root / "state.json"
        self.save()
        fake = self.bin / "gh"
        fake.write_text(FAKE_GH)
        fake.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "MAIN_GUARD_FIXTURE_STATE": str(self.state_path),
            "GH_TOKEN": "fixture-token-not-for-output",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_AUTHOR_EMAIL": "fixture-owner@example.invalid",
            "GIT_COMMITTER_EMAIL": "fixture-owner@example.invalid",
        }

    def call(self, args, **kwargs):
        return subprocess.run(
            args, text=True, capture_output=True, check=True, **kwargs
        ).stdout

    def git(self, *args):
        return self.call(
            ["git", "-c", "core.hooksPath=/dev/null", "-C", str(self.work), *args]
        )

    def save(self):
        self.state_path.write_text(json.dumps(self.state))

    def read(self):
        return json.loads(self.state_path.read_text())

    def guard(self, *args):
        self.assertEqual(
            self.git("remote", "get-url", "origin").strip(), self.origin.as_uri()
        )
        self.save()
        result = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).with_name("main_guard.py").resolve()),
                "--repo",
                g.REPOSITORY,
                "--run-id",
                "200",
                *args,
            ],
            cwd=self.work,
            env=self.env,
            text=True,
            capture_output=True,
        )
        self.state = self.read()
        self.assertNotIn("fixture-token-not-for-output", result.stdout + result.stderr)
        self.assertNotIn(
            "secret-not-for-output",
            result.stdout + result.stderr + json.dumps(self.state["writes"]),
        )
        return result

    def remote(self, ref):
        return self.call(
            ["git", "--git-dir", str(self.origin), "rev-parse", ref]
        ).strip()

    def test_default_and_explicit_dry_run_use_actual_git_without_remote_mutations(self):
        for args in ((), ("--dry-run",)):
            r = self.guard(*args)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("REVERT_PR", r.stdout)
            self.assertEqual(self.state["writes"], [])
            self.assertEqual(self.remote("refs/heads/main"), self.bad)
            self.assertEqual(self.git("rev-parse", "HEAD").strip(), self.bad)
            self.assertEqual(self.git("status", "--porcelain"), "")

    def test_apply_reverts_actual_tree_only_on_full_sha_candidate_branch(self):
        r = self.guard("--apply")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        branch = "refs/heads/revert/" + self.bad
        candidate = self.remote(branch)
        self.assertEqual(self.remote("refs/heads/main"), self.bad)
        tree = self.call(
            ["git", "--git-dir", str(self.origin), "rev-parse", candidate + "^{tree}"]
        ).strip()
        self.assertEqual(tree, self.git("rev-parse", self.good + "^{tree}").strip())
        author = self.call(
            [
                "git",
                "--git-dir",
                str(self.origin),
                "log",
                "-1",
                "--format=%an%n%ae%n%cn%n%ce%n%B",
                candidate,
            ]
        )
        identity = author.splitlines()
        self.assertEqual(identity[0], "Moncif")
        self.assertEqual(identity[2], "Moncif")
        self.assertEqual(
            identity[1], "41898282+github-actions[bot]@users.noreply.github.com"
        )
        self.assertEqual(identity[3], identity[1])
        self.assertNotIn("fixture-owner@example.invalid", author)
        self.assertIn("This reverts commit " + self.bad, author)
        writes = self.state["writes"]
        self.assertEqual(
            [w["path"].rsplit("/", 1)[1] for w in writes], ["issues", "pulls", "labels"]
        )
        self.assertIs(writes[-2]["fields"]["draft"], True)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual(
            len(self.git("worktree", "list", "--porcelain").split("worktree ")) - 1, 1
        )

    def test_stale_run_never_writes(self):
        self.call(
            [
                "git",
                "--git-dir",
                str(self.origin),
                "update-ref",
                "refs/heads/main",
                self.future,
            ]
        )
        r = self.guard("--apply")
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.state["writes"], [])
        self.assertIn("main or run attempt changed", r.stdout)

    def test_main_advance_during_collection_blocks_all_writes(self):
        self.state["advance_on_runs"] = True
        self.guard("--apply")
        self.assertEqual(self.state["writes"], [])
        self.assertEqual(self.remote("refs/heads/main"), self.future)

    def test_main_advance_after_issue_blocks_branch_and_pr(self):
        self.state["advance_after_issue"] = True
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))
        self.assertEqual(
            self.git("ls-remote", "origin", "refs/heads/revert/" + self.bad), ""
        )

    def test_wrong_workflow_path_event_repo_or_incomplete_run_is_not_acted_on(self):
        original = json.loads(json.dumps(self.state["run"]))
        for field, value in [
            ("workflow_id", 99),
            ("path", ".github/workflows/other.yml"),
            ("event", "pull_request"),
            ("status", "in_progress"),
            ("head_branch", "topic"),
            ("head_repository", {"full_name": "evil/fork"}),
        ]:
            self.state["run"] = {**original, field: value}
            self.guard("--apply")
            self.assertEqual(self.state["writes"], [])
        self.state["run"] = original

    def test_unknown_failed_logs_and_missing_steps_report_only(self):
        failed = next(j for j in self.state["jobs"] if j["name"] == "msrv")
        self.state["deny_logs"] = [str(failed["id"])]
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))

    def test_multi_commit_unknown_or_red_predecessor_is_not_guessed(self):
        self.state["runs"][-1]["head_sha"] = self.future
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))

    def test_absent_current_run_in_history_reports_only(self):
        self.state["runs"] = self.state["runs"][1:]
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)

    def test_optional_failure_never_supplies_attribution_or_logs(self):
        self.state["jobs"].append(
            {
                "id": 999,
                "name": "gate (macos-latest)",
                "conclusion": "failure",
                "steps": [],
            }
        )
        self.guard("--apply")
        self.assertFalse(any("/jobs/999/" in c["path"] for c in self.state["calls"]))
        self.assertEqual(len(self.state["prs"]), 1)

    def test_code_error_outside_changed_files_is_issue_only(self):
        key = next(iter(self.state["logs"]))
        self.state["logs"][key] = (
            "error[E0308]: mismatch\n --> crates/lattice-core/src/other.rs:9:3"
        )
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)

    def test_exact_bot_login_is_issue_only_not_spoofable_git_display_name(self):
        self.state["commit"]["author"]["login"] = "dependabot[bot]"
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)

    def test_existing_remote_branch_is_never_overwritten(self):
        ref = "refs/heads/revert/" + self.bad
        self.call(["git", "--git-dir", str(self.origin), "update-ref", ref, self.good])
        self.guard("--apply")
        self.assertEqual(self.remote(ref), self.good)
        self.assertEqual(len(self.state["writes"]), 1)

    def test_closed_exact_revert_pr_blocks_duplicate_publication(self):
        self.state["prs"] = [
            {
                "number": 5,
                "head": {
                    "ref": "revert/" + self.bad,
                    "repo": {"full_name": g.REPOSITORY},
                },
                "base": {"ref": "main"},
            }
        ]
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)

    def test_required_jobs_are_paginated(self):
        fillers = [
            {
                "id": 1000 + i,
                "name": f"optional-{i}",
                "conclusion": "failure",
                "steps": [],
            }
            for i in range(100)
        ]
        self.state["jobs"] = fillers + self.state["jobs"]
        self.guard("--dry-run")
        self.assertEqual(self.state["writes"], [])
        self.assertTrue(
            any(
                "jobs?" in c["path"] and "page=2" in c["path"]
                for c in self.state["calls"]
            )
        )

    def test_red_predecessor_is_not_treated_as_known_good(self):
        self.state["runs"][-1]["conclusion"] = "failure"
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))

    def test_missing_required_job_and_missing_failure_steps_report_only(self):
        self.state["jobs"] = [j for j in self.state["jobs"] if j["name"] != "typos"]
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertIn("unknown", self.state["writes"][0]["fields"]["body"])

    def test_required_infrastructure_failure_does_not_mutate(self):
        key = next(iter(self.state["logs"]))
        self.state["logs"][key] = "failed to download dependency: HTTP 503"
        self.guard("--apply")
        self.assertEqual(self.state["writes"], [])

    def test_unknown_commit_identity_does_not_guess_bot_or_human(self):
        self.state["commit"]["author"] = None
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 1)
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))

    def test_foreign_fork_pr_with_same_head_name_is_not_our_revert(self):
        self.state["prs"] = [
            {
                "number": 5,
                "head": {
                    "ref": "revert/" + self.bad,
                    "repo": {"full_name": "someone/fork"},
                },
                "base": {"ref": "main"},
            }
        ]
        self.guard("--apply")
        self.assertEqual(len(self.state["prs"]), 2)
        self.assertEqual(len(self.state["writes"]), 3)

    def test_exact_existing_tracking_marker_is_idempotent(self):
        self.state["issues"] = [
            {
                "number": 7,
                "title": "main is red: " + self.bad,
                "state": "open",
                "body": "<!-- main-guard:"
                + self.bad
                + " -->\n<!-- main-guard-run:200:1 -->",
            }
        ]
        self.guard("--apply")
        self.assertEqual(len(self.state["writes"]), 2)
        self.assertFalse(
            any(w["path"].endswith("/issues") for w in self.state["writes"])
        )

    def test_issue_with_similar_title_but_no_exact_marker_is_not_reused(self):
        self.state["issues"] = [
            {
                "number": 9,
                "title": "main is red: " + self.bad,
                "state": "open",
                "body": "unrelated report",
            }
        ]
        self.guard("--apply")
        self.assertTrue(self.state["writes"][0]["path"].endswith("/issues"))
        self.assertEqual(len(self.state["issues"]), 2)

    def test_newer_success_prevents_delayed_revert(self):
        newer = {
            **self.state["runs"][0],
            "id": 300,
            "created_at": "2099-01-01T00:00:00Z",
            "conclusion": "success",
        }
        self.state["runs"].insert(0, newer)
        result = self.guard("--apply")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.state["prs"]), 0)
        self.assertEqual(len(self.state["writes"]), 1)

    def test_other_active_owned_incidents_only_report(self):
        other = "a" * 40
        self.state["issues"] = [
            {
                "number": 9,
                "state": "open",
                "title": "main is red: " + other,
                "body": "<!-- main-guard:" + other + " -->",
            }
        ]
        result = self.guard("--apply")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.state["prs"]), 0)
        self.assertEqual(len(self.state["writes"]), 1)

    def test_active_owned_candidate_only_reports(self):
        self.state["prs"] = [
            {
                "state": "open",
                "head": {
                    "ref": "revert/" + "a" * 40,
                    "repo": {"full_name": g.REPOSITORY},
                },
                "base": {"ref": "main"},
                "labels": [{"name": "main-red"}],
            }
        ]
        result = self.guard("--apply")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.state["prs"]), 1)
        self.assertEqual(len(self.state["writes"]), 1)

    def test_racing_candidate_ref_is_protected_by_actual_git_create_lease(self):
        self.state["race_branch"] = True
        result = self.guard("--apply")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.remote("refs/heads/revert/" + self.bad), self.good)
        self.assertEqual(self.remote("refs/heads/main"), self.bad)
        self.assertEqual(len(self.state["prs"]), 0)
        comments = [
            w["fields"]["body"]
            for w in self.state["writes"]
            if w["path"].endswith("/comments")
        ]
        self.assertEqual(len(comments), 1)
        self.assertIn("Candidate publication failed", comments[0])
        self.assertNotIn("fixture-token", comments[0])
        self.assertNotIn("secret-not-for-output", comments[0])


if __name__ == "__main__":
    unittest.main()
