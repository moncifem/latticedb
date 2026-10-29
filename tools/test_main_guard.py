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
        self.assertFalse(g.infra_only({"steps": []}))

    def test_revert_message_shape(self):
        m = g.revert_message("abc123", "feat(lattice-core): " + "x" * 80)
        first = m.splitlines()[0]
        self.assertTrue(first.startswith("revert: "))
        self.assertLessEqual(len(first), 72)
        self.assertIn("This reverts commit abc123.", m)


if __name__ == "__main__":
    unittest.main()
