import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import main_guard as g  # noqa: E402
from main_guard import ISSUE_ONLY, NONE, REVERT_PR, decide


def job(name, conclusion="failure", infra=False):
    return {"name": name, "conclusion": conclusion, "infra": infra}


class DecisionTable(unittest.TestCase):
    def act(self, *a, **k):
        return decide(*a, **k).action

    def test_required_failure_reverts(self):
        for check in sorted(g.REQUIRED_CHECKS):
            with self.subTest(check=check):
                self.assertEqual(self.act("failure", [check], [job(check)], False, False), REVERT_PR)

    def test_reason_names_the_check(self):
        d = decide("failure", ["msrv"], [job("msrv")], False, False)
        self.assertIn("msrv", d.reason)

    def test_non_failure_runs_do_nothing(self):
        for c in ("success", "cancelled", "skipped", "timed_out", "neutral", "action_required", None):
            with self.subTest(conclusion=c):
                self.assertEqual(self.act(c, ["gate (ubuntu-latest)"], [], False, False), NONE)

    def test_macos_only_failure_does_nothing(self):
        self.assertEqual(
            self.act("failure", ["gate (macos-latest)"], [job("gate (macos-latest)")], False, False), NONE
        )

    def test_no_failed_checks_does_nothing(self):
        self.assertEqual(self.act("failure", [], [], False, False), NONE)

    def test_macos_plus_required_still_acts(self):
        names = ["gate (macos-latest)", "typos"]
        self.assertEqual(self.act("failure", names, [job(n) for n in names], False, False), REVERT_PR)

    def test_infra_failure_does_nothing(self):
        self.assertEqual(self.act("failure", ["msrv"], [job("msrv", infra=True)], False, False), NONE)

    def test_cancelled_or_timed_out_job_does_nothing(self):
        for c in ("cancelled", "timed_out", "skipped"):
            with self.subTest(job=c):
                self.assertEqual(self.act("failure", ["msrv"], [job("msrv", c)], False, False), NONE)

    def test_one_real_failure_among_infra_still_acts(self):
        jobs = [job("msrv", infra=True), job("typos")]
        self.assertEqual(self.act("failure", ["msrv", "typos"], jobs, False, False), REVERT_PR)

    def test_revert_of_revert_is_issue_only(self):
        self.assertEqual(self.act("failure", ["typos"], [job("typos")], True, False), ISSUE_ONLY)

    def test_second_revert_for_same_sha_is_issue_only(self):
        self.assertEqual(self.act("failure", ["typos"], [job("typos")], False, True), ISSUE_ONLY)

    def test_bot_only_change_is_issue_only(self):
        self.assertEqual(
            self.act("failure", ["typos"], [job("typos")], False, False, bot_only=True), ISSUE_ONLY
        )

    def test_unknown_culprit_is_issue_only(self):
        self.assertEqual(
            self.act("failure", ["typos"], [job("typos")], False, False, culprit_known=False), ISSUE_ONLY
        )

    def test_guards_do_not_override_none(self):
        self.assertEqual(self.act("cancelled", ["typos"], [], True, True, bot_only=True), NONE)
        self.assertEqual(self.act("failure", ["gate (macos-latest)"], [], True, True), NONE)


class Helpers(unittest.TestCase):
    def test_revert_subjects(self):
        for s in ('Revert "feat: x"', "revert: feat: x", "revert(core): y", "Revert foo"):
            self.assertTrue(g.is_revert_subject(s), s)
        for s in ("feat: add revert flag", "fix(x): reverting is hard", "docs: revert policy"):
            self.assertFalse(g.is_revert_subject(s), s)

    def test_single_commit_is_culprit(self):
        self.assertEqual(g.pick_culprit({"a": ["README.md"]}, set()), "a")

    def test_culprit_is_commit_touching_failing_crate(self):
        files = {
            "a": ["docs/x.md"],
            "b": ["crates/lattice-core/src/x.rs"],
            "c": ["crates/lattice-api/a.rs"],
        }
        self.assertEqual(g.pick_culprit(files, {"lattice-core"}), "b")

    def test_ambiguous_or_unknown_culprit(self):
        files = {"a": ["crates/lattice-core/x.rs"], "b": ["crates/lattice-core/y.rs"]}
        self.assertIsNone(g.pick_culprit(files, {"lattice-core"}))
        self.assertIsNone(g.pick_culprit(files, set()))
        self.assertIsNone(g.pick_culprit(files, {"lattice-api"}))

    def test_crates_from_log(self):
        log = "info: building lattice-sim\nerror: could not compile `lattice-core` (lib)\nok lattice_abi\n"
        self.assertEqual(g.crates_in_log(log), {"lattice-core"})

    def test_infra_only_steps(self):
        self.assertTrue(g.infra_only({"steps": [{"name": "Set up job", "conclusion": "failure"}]}))
        self.assertFalse(
            g.infra_only(
                {
                    "steps": [
                        {"name": "Set up job", "conclusion": "failure"},
                        {"name": "cargo xtask ci", "conclusion": "failure"},
                    ]
                }
            )
        )
        # A failed job with no failed step, or with a cancelled step, is not the code's fault.
        self.assertTrue(g.infra_only({"steps": []}))
        self.assertTrue(
            g.infra_only(
                {
                    "steps": [
                        {"name": "cargo xtask ci", "conclusion": "cancelled"},
                        {"name": "cargo xtask ci", "conclusion": "failure"},
                    ]
                }
            )
        )

    def test_root_manifest_change_makes_culprit_undeterminable(self):
        files = {"a": ["Cargo.lock"], "b": ["crates/lattice-core/x.rs"], "c": ["docs/x.md"]}
        self.assertIsNone(g.pick_culprit(files, {"lattice-core"}))
        # A single commit is still blamed even if it touches the manifest.
        self.assertEqual(g.pick_culprit({"a": ["Cargo.toml"]}, set()), "a")

    def test_clean_neutralises_markdown_and_mentions(self):
        out = g.clean("fix `x` @owner closes #12")
        self.assertNotIn("`", out)
        self.assertNotIn("@owner", out)
        self.assertNotIn("#12", out)

    def test_pr_body_flags_best_guess_and_cleans_subject(self):
        guess = g.pr_body("msrv", "https://run", "abc", "fix @a `b`", guess=True)
        self.assertIn("best guess", guess)
        self.assertNotIn("`b`", guess)
        self.assertNotIn("best guess", g.pr_body("msrv", "https://run", "abc", "s"))


def run_rec(rid, created, conclusion, name="pr", status="completed"):
    return {"id": rid, "created_at": created, "conclusion": conclusion, "name": name, "status": status}


class Baseline(unittest.TestCase):
    def test_previous_green_is_ok(self):
        runs = [run_rec(2, "2026-01-02", "failure"), run_rec(1, "2026-01-01", "success")]
        self.assertTrue(g.baseline_ok(runs, 2)[0])

    def test_previous_red_is_not_ok(self):
        runs = [run_rec(2, "2026-01-02", "failure"), run_rec(1, "2026-01-01", "failure")]
        self.assertFalse(g.baseline_ok(runs, 2)[0])

    def test_previous_cancelled_is_not_ok(self):
        runs = [run_rec(2, "2026-01-02", "failure"), run_rec(1, "2026-01-01", "cancelled")]
        self.assertFalse(g.baseline_ok(runs, 2)[0])

    def test_newer_green_run_is_not_ok(self):
        runs = [
            run_rec(3, "2026-01-03", "success"),
            run_rec(2, "2026-01-02", "failure"),
            run_rec(1, "2026-01-01", "success"),
        ]
        self.assertFalse(g.baseline_ok(runs, 2)[0])

    def test_no_history_or_unknown_run_is_not_ok(self):
        self.assertFalse(g.baseline_ok([run_rec(2, "2026-01-02", "failure")], 2)[0])
        self.assertFalse(g.baseline_ok([run_rec(1, "2026-01-01", "success")], 9)[0])

    def test_other_workflows_are_ignored(self):
        runs = [
            run_rec(2, "2026-01-02", "failure"),
            run_rec(5, "2026-01-01T12", "failure", name="other"),
            run_rec(1, "2026-01-01", "success"),
        ]
        self.assertTrue(g.baseline_ok(runs, 2)[0])

    def test_in_progress_newer_run_does_not_count(self):
        runs = [
            run_rec(3, "2026-01-03", None, status="in_progress"),
            run_rec(2, "2026-01-02", "failure"),
            run_rec(1, "2026-01-01", "success"),
        ]
        self.assertTrue(g.baseline_ok(runs, 2)[0])


class BaselineDecision(unittest.TestCase):
    def test_red_baseline_is_issue_only(self):
        d = decide("failure", ["typos"], [job("typos")], False, False, baseline_green=False)
        self.assertEqual(d.action, ISSUE_ONLY)

    def test_baseline_does_not_override_none(self):
        d = decide("failure", ["gate (macos-latest)"], [], False, False, baseline_green=False)
        self.assertEqual(d.action, NONE)

    def test_default_baseline_reverts(self):
        self.assertEqual(decide("failure", ["typos"], [job("typos")], False, False).action, REVERT_PR)


class RevertFailure(unittest.TestCase):
    def test_conflict_aborts_and_returns_error(self):
        import subprocess
        from unittest import mock

        calls = []

        def fake_run(cmd, check=True, capture=True):
            calls.append(cmd[:3])
            if cmd[:3] == ["git", "ls-remote", "origin"]:
                return ""
            if cmd[:2] == ["git", "revert"] and "--abort" not in cmd:
                raise subprocess.CalledProcessError(1, cmd, stderr="CONFLICT in x.rs")
            return ""

        with mock.patch.object(g, "run", fake_run):
            err = g.do_revert("abc", "feat: x", "revert/abc")
        self.assertIn("CONFLICT", err)
        self.assertIn(["git", "revert", "--abort"], calls)
        self.assertFalse(any(c[:2] == ["git", "push"] for c in calls))

    def test_push_rejection_returns_error(self):
        import subprocess
        from unittest import mock

        def fake_run(cmd, check=True, capture=True):
            if cmd[:2] == ["git", "push"]:
                raise subprocess.CalledProcessError(1, cmd, stderr="refusing to allow workflow update")
            return ""

        with mock.patch.object(g, "run", fake_run), mock.patch.object(g.subprocess, "run") as sub:
            sub.return_value = None
            err = g.do_revert("abc", "feat: x", "revert/abc")
        self.assertIn("workflow", err)

    def test_existing_remote_branch_skips_revert_and_push(self):
        from unittest import mock

        calls = []

        def fake_run(cmd, check=True, capture=True):
            calls.append(cmd[:2])
            return "deadbeef\trefs/heads/revert/abc\n" if cmd[:2] == ["git", "ls-remote"] else ""

        with mock.patch.object(g, "run", fake_run):
            self.assertIsNone(g.do_revert("abc", "feat: x", "revert/abc"))
        self.assertEqual(calls, [["git", "ls-remote"]])

    def test_revert_message_shape(self):
        m = g.revert_message("abc123", "feat(lattice-core): " + "x" * 80)
        first = m.splitlines()[0]
        self.assertTrue(first.startswith("revert: "))
        self.assertLessEqual(len(first), 72)
        self.assertIn("This reverts commit abc123.", m)


if __name__ == "__main__":
    unittest.main()
