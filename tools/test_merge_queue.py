#!/usr/bin/env python3
"""Tests for merge_queue.py. The queue runs against a fake `gh` that is put first on PATH; nothing here
contacts GitHub. The fake keeps a scripted answer list per command and pull request: each call takes the
next answer and the last one repeats, and every call is recorded so a test can assert what was (not) run."""
import io
import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

import merge_queue as mq

REQUIRED = ["gate (ubuntu-latest)", "gate (ubuntu-24.04-arm)", "secret-scan", "typos", "commit-messages", "msrv"]
H1, H2, H3 = "1" * 40, "2" * 40, "3" * 40

FAKE_GH = r'''#!%(python)s
import json, os, sys
path = os.environ["FAKE_STATE"]
s = json.load(open(path))
args = sys.argv[1:]
s.setdefault("calls", []).append(args)

def nxt(items):
    value = items[0]
    if len(items) > 1:
        items.pop(0)
    return value

def save():
    json.dump(s, open(path, "w"))

def reply(out="", rc=0, err=""):
    save()
    sys.stdout.write(out)
    sys.stderr.write(err)
    sys.exit(rc)

if args[:2] == ["pr", "list"]:
    reply(json.dumps(s.get("list", [])))
if args[:2] == ["pr", "view"]:
    pr = s["prs"][args[2]]
    item = nxt(pr["view"])
    if "api_rc" in item:
        reply("", item["api_rc"], "HTTP 502")
    reply(json.dumps(item))
if args[:2] == ["pr", "checks"]:
    pr = s["prs"][args[2]]
    item = nxt(pr["checks"])
    reply(item["raw"] if "raw" in item else json.dumps(item["json"]), item.get("rc", 0))
if args[:2] == ["pr", "update-branch"]:
    pr = s["prs"][args[2]]
    item = nxt(pr.get("update", [{"rc": 0}]))
    reply("", item.get("rc", 0), item.get("text", ""))
if args[:2] == ["run", "rerun"]:
    item = nxt(s.get("rerun", [{"rc": 0}]))
    reply("", item.get("rc", 0))
if args[:2] == ["pr", "merge"]:
    pr = s["prs"][args[2]]
    item = nxt(pr.get("merge", [{"rc": 0, "merged": True}]))
    if item.get("merged"):
        done = dict(pr["view"][-1])
        done["state"] = "MERGED"
        pr["view"] = [done]
    reply("", item.get("rc", 0), item.get("text", ""))
reply("", 99, "fake gh: unexpected " + " ".join(args))
'''


def view(head=H1, state="OPEN", draft=False, merge="CLEAN", base="main", labels=(), bot=False):
    return {"state": state, "isDraft": draft, "mergeStateStatus": merge, "headRefOid": head,
            "headRefName": "feature", "baseRefName": base, "title": "t",
            "labels": [{"name": name} for name in labels],
            "author": {"login": "app/dependabot" if bot else "someone", "is_bot": bot}}


def check(name, bucket="pass", run="100", started="2026-10-30T10:00:00Z"):
    return {"name": name, "bucket": bucket, "workflow": "pr",
            "link": f"https://github.com/o/r/actions/runs/{run}/job/1", "startedAt": started}


def all_checks(**buckets):
    return [check(name, buckets.get(name, "pass")) for name in REQUIRED]


def ans(checks, rc=0):
    return {"json": checks, "rc": rc}


GREEN = ans(all_checks())
FAIL_TYPOS = ans(all_checks(typos="fail"), rc=1)
PENDING = ans(all_checks(msrv="pending"), rc=8)


class QueueCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        shim = root / "bin"
        shim.mkdir()
        gh = shim / "gh"
        gh.write_text(FAKE_GH % {"python": sys.executable})
        gh.chmod(gh.stat().st_mode | stat.S_IEXEC)
        self.state_path = root / "state.json"
        self.status = root / "status.json"
        self.logfile = root / "queue.log"
        old = {k: os.environ.get(k) for k in ("PATH", "FAKE_STATE")}
        os.environ["PATH"] = f"{shim}{os.pathsep}{old['PATH']}"
        os.environ["FAKE_STATE"] = str(self.state_path)
        self.addCleanup(self.restore, old)
        self.state = {"prs": {}}

    @staticmethod
    def restore(old):
        for key, value in old.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def pr(self, n, views, checks=(GREEN,), **extra):
        self.state["prs"][str(n)] = {"view": list(views), "checks": list(checks), **extra}

    def save(self):
        self.state_path.write_text(json.dumps(self.state))

    def calls(self):
        return json.loads(self.state_path.read_text()).get("calls", [])

    def calls_of(self, *prefix):
        return [c for c in self.calls() if c[: len(prefix)] == list(prefix)]

    def merges(self):
        return [c[2] for c in self.calls_of("pr", "merge")]

    def make(self, **kw):
        self.save()
        base = dict(poll=0.0, settle=0.0, per_pr_timeout=3.0, rerun_grace=0.0, round_sleep=0.0, rounds=2,
                    status_file=str(self.status), log_file=str(self.logfile))
        base.update(kw)
        self.out = io.StringIO()
        q = mq.Queue(mq.GhApi(), sorted(REQUIRED), mq.Options(**base), out=self.out)
        return q

    def status_of(self, n):
        return json.loads(self.status.read_text())["prs"][str(n)]


class FlowTests(QueueCase):
    def test_clean_green_pr_merges_on_the_exact_head(self):
        self.pr(5, [view()])
        q = self.make()
        self.assertEqual(q.process(5), "merged")
        merge = self.calls_of("pr", "merge")[0]
        self.assertEqual(merge[:3], ["pr", "merge", "5"])
        self.assertIn("--rebase", merge)
        self.assertEqual(merge[merge.index("--match-head-commit") + 1], H1)
        self.assertNotIn("--admin", merge)
        self.assertNotIn("--delete-branch", merge)

    def test_flaky_check_passes_on_rerun(self):
        self.pr(5, [view()], checks=[FAIL_TYPOS, FAIL_TYPOS, GREEN])
        q = self.make(rerun_grace=5.0)  # the second failure is the stale pre-rerun result
        self.assertEqual(q.process(5), "merged")
        self.assertEqual(self.calls_of("run", "rerun"), [["run", "rerun", "100", "--failed"]])
        self.assertEqual(len(self.calls_of("pr", "merge")), 1)

    def test_real_failure_is_parked_with_the_check_names(self):
        self.pr(5, [view()], checks=[FAIL_TYPOS])
        q = self.make()
        self.assertEqual(q.process(5), "parked")
        self.assertEqual(len(self.calls_of("run", "rerun")), 1)  # re-run once, not twice
        self.assertEqual(self.merges(), [])
        entry = self.status_of(5)
        self.assertEqual(entry["state"], "parked")
        self.assertEqual(entry["failed_checks"], ["typos"])
        self.assertIn("typos", entry["reason"])

    def test_failed_status_check_without_a_run_is_parked_not_rerun(self):
        bad = [dict(check("typos", "fail"), link="https://example.com/status")] + all_checks()[1:]
        self.pr(5, [view()], checks=[ans(bad, rc=1)])
        self.assertEqual(self.make().process(5), "parked")
        self.assertEqual(self.calls_of("run", "rerun"), [])
        self.assertEqual(self.merges(), [])

    def test_rerun_refused_is_parked(self):
        self.pr(5, [view()], checks=[FAIL_TYPOS])
        self.state["rerun"] = [{"rc": 1}]
        self.assertEqual(self.make().process(5), "parked")
        self.assertEqual(self.merges(), [])

    def test_pending_checks_are_waited_for(self):
        self.pr(5, [view()], checks=[PENDING, PENDING, GREEN])
        self.assertEqual(self.make().process(5), "merged")

    def test_pending_beats_failed_so_everything_finishes_first(self):
        mixed = ans(all_checks(typos="fail", msrv="pending"), rc=8)
        self.pr(5, [view()], checks=[mixed, GREEN])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(self.calls_of("run", "rerun"), [])

    def test_api_error_is_unknown_not_a_result(self):
        self.pr(5, [{"api_rc": 1}, view()],
                checks=[{"raw": "", "rc": 1}, {"raw": "not json", "rc": 0}, {"raw": "[]", "rc": 0}, GREEN])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(self.calls_of("run", "rerun"), [])

    def test_persistent_api_error_never_merges(self):
        self.pr(5, [view()], checks=[{"raw": "", "rc": 1}])
        q = self.make(per_pr_timeout=0.4)
        self.assertEqual(q.process(5), "parked")
        self.assertEqual(self.merges(), [])

    def test_failure_exit_code_with_garbage_stdout_is_unknown(self):
        self.pr(5, [view()], checks=[{"raw": "oops", "rc": 1}])
        q = self.make(per_pr_timeout=0.3)
        self.assertEqual(q.process(5), "parked")
        self.assertEqual(self.calls_of("run", "rerun"), [])

    def test_missing_required_check_never_merges(self):
        partial = ans([check("typos"), check("msrv")])
        self.pr(5, [view()], checks=[partial])
        q = self.make(per_pr_timeout=0.4)
        self.assertEqual(q.process(5), "parked")
        self.assertEqual(self.merges(), [])
        self.assertIn("timed out", self.status_of(5)["reason"])

    def test_skipped_optional_check_is_fine_but_skipped_required_is_not(self):
        extra = all_checks() + [check("optional job", "skipping")]
        self.pr(5, [view()], checks=[ans(extra)])
        self.assertEqual(self.make().process(5), "merged")
        skipped = ans(all_checks(msrv="skipping"))
        self.pr(6, [view()], checks=[skipped])
        self.assertEqual(self.make(per_pr_timeout=0.3).process(6), "parked")

    def test_head_moved_during_the_wait_restarts_on_the_new_head(self):
        self.pr(5, [view(H1), view(H2)], checks=[PENDING, GREEN])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(self.merges(), ["5"])
        merge = self.calls_of("pr", "merge")[0]
        self.assertEqual(merge[merge.index("--match-head-commit") + 1], H2)

    def test_head_moving_right_before_the_merge_is_not_merged_on_the_old_head(self):
        # process view, wait view, pre-merge view (moved), then the new head all the way through.
        self.pr(5, [view(H1), view(H1), view(H2)], checks=[GREEN])
        self.assertEqual(self.make().process(5), "merged")
        merge = self.calls_of("pr", "merge")[0]
        self.assertEqual(merge[merge.index("--match-head-commit") + 1], H2)

    def test_checks_turning_red_before_the_merge_block_it(self):
        # green in the wait, red at the pre-merge read, then red again after the single re-run.
        self.pr(5, [view()], checks=[GREEN, FAIL_TYPOS])
        self.assertEqual(self.make().process(5), "parked")
        self.assertEqual(self.merges(), [])

    def test_behind_branch_is_updated_then_checked_on_the_new_head(self):
        self.pr(5, [view(H1, merge="BEHIND"), view(H2)], checks=[GREEN])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(self.calls_of("pr", "update-branch"), [["pr", "update-branch", "5", "--rebase"]])
        merge = self.calls_of("pr", "merge")[0]
        self.assertEqual(merge[merge.index("--match-head-commit") + 1], H2)

    def test_up_to_date_branch_is_not_updated(self):
        self.pr(5, [view()])
        self.make().process(5)
        self.assertEqual(self.calls_of("pr", "update-branch"), [])

    def test_update_conflict_is_skipped(self):
        self.pr(5, [view(merge="BEHIND")], update=[{"rc": 1, "text": "merge conflict between base and head"}])
        self.assertEqual(self.make().process(5), "skipped")
        self.assertEqual(self.merges(), [])

    def test_dirty_pr_is_skipped_without_touching_it(self):
        self.pr(5, [view(merge="DIRTY")])
        self.assertEqual(self.make().process(5), "skipped")
        self.assertEqual([c[:2] for c in self.calls()], [["pr", "view"]])
        self.assertIn("conflicts", self.status_of(5)["reason"])

    def test_merge_refused_then_retried(self):
        self.pr(5, [view()], merge=[{"rc": 1, "text": "base branch was modified"}, {"rc": 0, "merged": True}])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(len(self.calls_of("pr", "merge")), 2)

    def test_merge_that_exits_zero_but_is_not_merged_is_retried(self):
        self.pr(5, [view()], merge=[{"rc": 0, "merged": False}, {"rc": 0, "merged": True}])
        self.assertEqual(self.make().process(5), "merged")
        self.assertEqual(len(self.calls_of("pr", "merge")), 2)

    def test_merge_refused_every_time_gives_up_and_parks(self):
        self.pr(5, [view()], merge=[{"rc": 1, "text": "no"}])
        self.assertEqual(self.make().process(5), "parked")
        self.assertEqual(len(self.calls_of("pr", "merge")), mq.MAX_ATTEMPTS)

    def test_closed_pr_is_done(self):
        self.pr(5, [view(state="MERGED")])
        self.assertEqual(self.make().process(5), "closed")

    def test_pr_merged_by_someone_else_during_the_wait(self):
        self.pr(5, [view(), view(state="MERGED")], checks=[PENDING])
        self.assertEqual(self.make().process(5), "closed")
        self.assertEqual(self.merges(), [])

    def test_timeout_parks(self):
        self.pr(5, [view()], checks=[PENDING])
        self.assertEqual(self.make(per_pr_timeout=0.3).process(5), "parked")
        self.assertEqual(self.merges(), [])


class ReadinessTests(QueueCase):
    def test_not_ready_pull_requests_are_skipped_untouched(self):
        cases = {"draft": view(draft=True), "hold": view(labels=["hold"]), "base": view(base="release"),
                 "bot": view(bot=True)}
        for n, (name, snapshot) in enumerate(cases.items(), start=10):
            self.pr(n, [snapshot])
        q = self.make()
        for n, name in enumerate(cases, start=10):
            self.assertEqual(q.process(n), "skipped", name)
        self.assertEqual(self.merges(), [])
        self.assertEqual({c[1] for c in self.calls()}, {"view"})

    def test_a_bot_pr_named_on_the_command_line_is_processed(self):
        self.pr(5, [view(bot=True)])
        self.assertEqual(self.make().process(5, listed=True), "merged")

    def test_readiness_pure(self):
        self.assertEqual(mq.readiness(view())[0], "ready")
        self.assertEqual(mq.readiness(view(state="CLOSED")), ("closed", "closed"))
        self.assertEqual(mq.readiness(view(labels=["x", "hold"]))[0], "skip")
        self.assertEqual(mq.readiness(view(labels=["hold"]), hold_label="")[0], "ready")
        self.assertEqual(mq.readiness(view(merge="BEHIND"))[0], "ready")
        self.assertEqual(mq.readiness(view(merge="DIRTY"))[0], "skip")

    def test_parse_pr_view_rejects_incomplete_snapshots(self):
        good = json.dumps(view())
        self.assertIsNotNone(mq.parse_pr_view(good))
        self.assertIsNone(mq.parse_pr_view(""))
        self.assertIsNone(mq.parse_pr_view("[]"))
        self.assertIsNone(mq.parse_pr_view(json.dumps({**view(), "headRefOid": "abc"})))
        broken = view()
        del broken["mergeStateStatus"]
        self.assertIsNone(mq.parse_pr_view(json.dumps(broken)))


class ChecksTests(unittest.TestCase):
    def test_classification(self):
        req = sorted(REQUIRED)
        self.assertEqual(mq.classify_checks(None, req), ("unknown", []))
        self.assertEqual(mq.classify_checks([], req), ("unknown", []))
        self.assertEqual(mq.classify_checks(all_checks(), req), ("green", []))
        self.assertEqual(mq.classify_checks(all_checks(typos="fail"), req), ("failed", ["typos"]))
        self.assertEqual(mq.classify_checks(all_checks(typos="cancel"), req), ("failed", ["typos"]))
        self.assertEqual(mq.classify_checks(all_checks(typos="pending"), req), ("pending", ["typos"]))
        self.assertEqual(mq.classify_checks(all_checks(typos="weird"), req), ("pending", ["typos"]))
        verdict, names = mq.classify_checks([check("typos")], req)
        self.assertEqual(verdict, "pending")
        self.assertEqual(len(names), 5)

    def test_latest_run_of_a_check_wins(self):
        old_fail = check("typos", "fail", started="2026-10-30T09:00:00Z")
        new_pass = check("typos", "pass", started="2026-10-30T10:00:00Z")
        rest = [c for c in all_checks() if c["name"] != "typos"]
        self.assertEqual(mq.classify_checks(rest + [old_fail, new_pass], REQUIRED)[0], "green")
        old_pass = check("typos", "pass", started="2026-10-30T09:00:00Z")
        new_fail = check("typos", "fail", started="2026-10-30T10:00:00Z")
        self.assertEqual(mq.classify_checks(rest + [old_pass, new_fail], REQUIRED)[0], "failed")

    def test_failed_run_ids_are_unique_and_sorted(self):
        checks = [check("a", "fail", run="22"), check("b", "fail", run="22"), check("c", "fail", run="7"),
                  check("d", "pass", run="9")]
        self.assertEqual(mq.failed_run_ids(checks), ["22", "7"])

    def test_required_contexts_from_the_real_protection_file(self):
        config = json.loads(mq.DEFAULT_REQUIRED_FILE.read_text())
        self.assertEqual(mq.required_contexts(config), sorted(REQUIRED))
        self.assertIsNone(mq.required_contexts({}))
        self.assertIsNone(mq.required_contexts({"required_status_checks": {"contexts": []}}))
        self.assertIsNone(mq.required_contexts({"required_status_checks": {"contexts": [1]}}))


class OrderingTests(QueueCase):
    def test_discovery_sorts_and_filters(self):
        prs = [{"number": 9, "isDraft": False, "labels": [], "author": {"login": "a"}},
               {"number": 3, "isDraft": False, "labels": [], "author": {"login": "a"}},
               {"number": 4, "isDraft": True, "labels": [], "author": {"login": "a"}},
               {"number": 5, "isDraft": False, "labels": [{"name": "hold"}], "author": {"login": "a"}},
               {"number": 6, "isDraft": False, "labels": [], "author": {"login": "app/dependabot", "is_bot": True}},
               {"number": 3, "isDraft": False, "labels": [], "author": {"login": "a"}}, "junk"]
        self.assertEqual(mq.discovery_order(prs), [3, 9])
        self.assertEqual(mq.discovery_order(prs, include_bots=True), [3, 6, 9])

    def test_explicit_order_is_kept_without_duplicates(self):
        self.assertEqual(mq.explicit_order([7, 3, 7, 5]), [7, 3, 5])

    def test_queue_merges_in_the_given_order_and_a_parked_pr_does_not_block(self):
        self.pr(7, [view(H1)])
        self.pr(3, [view(H2)], checks=[FAIL_TYPOS])
        self.pr(5, [view(H3)])
        q = self.make()
        left = q.run([7, 3, 5], once=True)
        self.assertEqual(self.merges(), ["7", "5"])
        self.assertEqual(left, [3])

    def test_discovered_prs_run_lowest_number_first(self):
        self.state["list"] = [{"number": n, "isDraft": False, "labels": [], "author": {"login": "a"}}
                              for n in (12, 4, 8)]
        for n in (12, 4, 8):
            self.pr(n, [view()])
        q = self.make()
        self.assertEqual(q.run([], once=True), [])
        self.assertEqual(self.merges(), ["4", "8", "12"])

    def test_next_round_picks_up_a_fixed_branch(self):
        # first round: red; the author fixes it; second round: green.
        self.pr(5, [view(H1), view(H1), view(H1), view(H1), view(H2)], checks=[FAIL_TYPOS, FAIL_TYPOS, GREEN])
        q = self.make(rounds=3)
        self.assertEqual(q.run([5], once=False), [])
        self.assertEqual(self.merges(), ["5"])
        self.assertEqual(self.status_of(5)["round"], 2)

    def test_once_does_not_retry(self):
        self.pr(5, [view()], checks=[FAIL_TYPOS])
        q = self.make(rounds=5)
        self.assertEqual(q.run([5], once=True), [5])
        self.assertEqual(len(self.calls_of("run", "rerun")), 1)

    def test_loop_stops_at_the_round_limit(self):
        self.pr(5, [view(draft=True)])
        q = self.make(rounds=3)
        self.assertEqual(q.run([5], once=False), [5])
        self.assertEqual(len([c for c in self.calls() if c[:2] == ["pr", "view"]]), 3)

    def test_listing_error_in_discovery_is_not_an_empty_queue(self):
        self.pr(5, [view()])
        q = self.make()
        q.api.list_open = lambda base: None
        self.assertEqual(q.run([], once=True), [])
        self.assertIn("could not list", self.out.getvalue())


class StatusTests(QueueCase):
    def test_status_file_is_written_after_each_decision(self):
        self.pr(5, [view()])
        self.pr(6, [view(draft=True)])
        q = self.make()
        seen = []
        original = q.write_status

        def spy():
            original()
            seen.append(sorted(json.loads(self.status.read_text())["prs"]))
        q.write_status = spy
        q.run([5, 6], once=True)
        self.assertIn(["5"], seen)          # after PR 5's decision, before PR 6's
        data = json.loads(self.status.read_text())
        self.assertEqual(sorted(data["prs"]), ["5", "6"])
        self.assertEqual(data["prs"]["5"]["state"], "merged")
        self.assertEqual(data["prs"]["5"]["head"], H1)
        self.assertEqual(data["prs"]["6"]["state"], "skipped")
        self.assertEqual(data["prs"]["6"]["reason"], "draft")
        self.assertTrue(data["finished"])
        self.assertEqual(data["left"], [6])
        self.assertIsNone(data["current"])
        self.assertTrue(data["updated"].endswith("Z"))

    def test_log_has_one_line_per_decision(self):
        self.pr(5, [view()])
        self.make().process(5)
        text = self.logfile.read_text()
        self.assertIn("PR 5: MERGED", text)
        self.assertEqual(text.strip().splitlines()[-1], self.out.getvalue().strip().splitlines()[-1])

    def test_no_temp_files_are_left_beside_the_status_file(self):
        self.pr(5, [view()])
        self.make().process(5)
        self.assertEqual(sorted(p.name for p in self.status.parent.glob(".status-*")), [])


class CommandLineTests(QueueCase):
    def run_main(self, *args):
        self.save()
        buf = io.StringIO()
        old = sys.stdout
        sys.stdout = buf
        try:
            code = mq.main(["--poll", "0", "--settle", "0", "--round-sleep", "0", "--rerun-grace", "0",
                            "--status-file", str(self.status), "--log-file", str(self.logfile), *args])
        finally:
            sys.stdout = old
        return code, buf.getvalue()

    def test_dry_run_reads_only(self):
        self.pr(5, [view()], checks=[FAIL_TYPOS])
        self.pr(6, [view(merge="BEHIND")])
        self.pr(7, [view()], checks=[GREEN])
        code, out = self.run_main("--dry-run", "5", "6", "7")
        self.assertEqual(code, 0)
        self.assertEqual([c[1] for c in self.calls()], ["view", "checks", "view", "view", "checks"])
        self.assertEqual(self.calls_of("pr", "merge") + self.calls_of("pr", "update-branch")
                         + self.calls_of("run", "rerun"), [])
        data = json.loads(self.status.read_text())
        self.assertTrue(data["dry_run"])
        self.assertIn("would re-run once", data["prs"]["5"]["reason"])
        self.assertIn("would update", data["prs"]["6"]["reason"])
        self.assertIn("would merge", data["prs"]["7"]["reason"])

    def test_once_exit_codes(self):
        self.pr(5, [view()])
        self.assertEqual(self.run_main("--once", "5")[0], 0)
        self.pr(6, [view(draft=True)])
        self.assertEqual(self.run_main("--once", "6")[0], 1)

    def test_refuses_to_run_without_required_checks(self):
        empty = Path(self.tmp.name) / "none.json"
        empty.write_text("{}")
        self.pr(5, [view()])
        code, _ = self.run_main("--required-file", str(empty), "5")
        self.assertEqual(code, 2)
        self.assertEqual(self.calls(), [])

    def test_default_status_paths_follow_tmpdir(self):
        old = os.environ.get("TMPDIR")
        os.environ["TMPDIR"] = self.tmp.name
        try:
            status, log = mq.default_paths()
        finally:
            if old is None:
                os.environ.pop("TMPDIR")
            else:
                os.environ["TMPDIR"] = old
        self.assertEqual(status, Path(self.tmp.name) / "lattice-merge-queue" / "status.json")
        self.assertEqual(log.name, "queue.log")


if __name__ == "__main__":
    unittest.main()
