"""`board run` publishes a live-activity row for the child it supervises (BOARD_ACTIVITY=1), so an agent that has
no hook still shows on the console. The runner only decides WHEN to call `board activity`; the helper owns the
rate limit and the rules (test_activity_cli.py). The expectations come from CONVENTIONS.md ("Agent activity").

Run: python3 -m unittest discover -s integrations/board/tests -p "test_activity_run.py"
"""

import os
import re
import sys
import unittest

import test_run
from test_run import RunnerCase, task


class Publishing(RunnerCase):
    def activity_calls(self):
        return self.calls("activity")

    def option(self, argv, name):
        return argv[argv.index(name) + 1]

    def run_ok(self, extra_env=None, code="pass", **kw):
        self.conf(tasks=[task()], **kw)
        child = self.script("child.py", code + "\n")
        p = self.run_runner(self.args("--owner", "w1", "--lease", "30", "--", sys.executable, child),
                            extra_env=extra_env)
        return p

    def test_nothing_is_published_unless_the_switch_is_one(self):
        for value in (None, "", "0", "yes"):
            self.conf(tasks=[task()])
            extra = {} if value is None else {"BOARD_ACTIVITY": value}
            child = self.script("c.py", "pass\n")
            p = self.run_runner(self.args("--owner", "w1", "--", sys.executable, child), extra_env=extra)
            self.assertEqual(p.returncode, 0, p.err)
            self.assertEqual(self.activity_calls(), [], value)
            names = [c["argv"][0] for c in self.calls()]
            self.assertEqual(names[:4], ["next", "get", "note", "done"])
            os.unlink(os.path.join(self.fb, "calls.jsonl"))
            os.unlink(os.path.join(self.fb, "state.json"))

    def test_a_task_is_published_as_working_then_idle_then_the_runner_ends(self):
        p = self.run_ok({"BOARD_ACTIVITY": "1"})
        self.assertEqual(p.returncode, 0, p.err)
        calls = self.activity_calls()
        label = re.sub(r"[^A-Za-z0-9._-]", "", os.path.basename(sys.executable))[:48]
        session = "run-%d" % p.pid
        self.assertEqual(calls[0]["argv"], ["activity", "--tool", "agent-run", "--target", label, "--outcome", "ok",
                                            "--state", "working", "--session", session, "--task", "t1", "--flush"])
        self.assertEqual(calls[1]["argv"], ["activity", "--tool", "agent-run", "--target", label, "--outcome", "ok",
                                            "--state", "idle", "--session", session, "--flush"])
        self.assertEqual(calls[-1]["argv"], ["activity", "--tool", "agent-run", "--target", label, "--outcome", "ok",
                                             "--state", "ended", "--session", session, "--flush"])
        self.assertEqual(len(calls), 3)
        for c in calls:
            self.assertEqual(c["author"], "w1", "published as the worker's name")

    def test_the_child_is_published_as_working_before_it_starts_and_idle_after_the_mark(self):
        self.run_ok({"BOARD_ACTIVITY": "1"})
        order = [c["argv"][0] if c["argv"][0] != "activity" else "activity:" + self.option(c["argv"], "--state")
                 for c in self.calls()]
        self.assertLess(order.index("activity:working"), order.index("done"))
        self.assertLess(order.index("done"), order.index("activity:idle"))
        self.assertEqual(order[-1], "activity:ended")

    def test_the_outcome_of_a_failed_child_is_error(self):
        p = self.run_ok({"BOARD_ACTIVITY": "1"}, code="import sys; sys.exit(3)")
        self.assertEqual(p.returncode, 0, p.err)
        idle = [c for c in self.activity_calls() if self.option(c["argv"], "--state") == "idle"][0]
        self.assertEqual(self.option(idle["argv"], "--outcome"), "error")
        self.assertNotIn("--task", idle["argv"], "an ended task is not linked")

    def test_a_failure_to_publish_never_changes_the_task_outcome(self):
        p = self.run_ok({"BOARD_ACTIVITY": "1"}, activity_rc=1)
        self.assertEqual(p.returncode, 0, p.err)
        self.assertEqual(len(self.calls("done")), 1)
        self.assertEqual(p.err.count("could not record activity"), 1, "the warning is printed once, not per call")

    def test_the_label_is_the_command_base_name_made_safe(self):
        self.conf(tasks=[task()])
        child = self.script("we ird;name.py", "pass\n")
        self.run_runner(self.args("--owner", "w1", "--", sys.executable, child), extra_env={"BOARD_ACTIVITY": "1"})
        # the label names the program that runs (the interpreter), whatever its arguments are
        label = self.option(self.activity_calls()[0]["argv"], "--target")
        self.assertRegex(label, r"^[A-Za-z0-9._-]{1,48}$")

    def test_a_command_with_odd_characters_is_cleaned(self):
        prog = self.script("we ird;na$me.py", "#!/bin/sh\nexit 0\n")
        self.conf(tasks=[task()])
        self.run_runner(self.args("--owner", "w1", "--", prog), extra_env={"BOARD_ACTIVITY": "1"})
        self.assertEqual(self.option(self.activity_calls()[0]["argv"], "--target"), "weirdname.py")

    def test_the_session_is_the_same_for_every_task_of_one_runner(self):
        self.conf(tasks=[task("t1"), task("t2"), task("t3")])
        child = self.script("c.py", "pass\n")
        p = self.run_runner(self.args("--owner", "w1", "--", sys.executable, child), extra_env={"BOARD_ACTIVITY": "1"})
        sessions = {self.option(c["argv"], "--session") for c in self.activity_calls()}
        self.assertEqual(sessions, {"run-%d" % p.pid})
        tasks = [self.option(c["argv"], "--task") for c in self.activity_calls() if "--task" in c["argv"]]
        self.assertEqual(tasks, ["t1", "t2", "t3"])

    def test_a_long_task_is_refreshed_at_each_heartbeat(self):
        child = self.script("slow.py", "import time; time.sleep(2.6)\n")
        self.conf(tasks=[task()])
        p = self.run_runner(self.args("--owner", "w1", "--lease", "3", "--", sys.executable, child),
                            extra_env={"BOARD_ACTIVITY": "1"})
        self.assertEqual(p.returncode, 0, p.err)
        refreshes = [c for c in self.activity_calls()
                     if self.option(c["argv"], "--state") == "working" and "--flush" not in c["argv"]]
        self.assertGreaterEqual(len(refreshes), 1, "a heartbeat every second must refresh the row")
        for c in refreshes:
            self.assertEqual(self.option(c["argv"], "--task"), "t1")

    def test_the_child_never_sees_the_switch(self):
        out = self.sandbox()
        child = self.script("dump.py", """
            import json, os
            open(%r, "w").write(json.dumps(sorted(k for k in os.environ if k.startswith("BOARD_"))))
        """ % os.path.join(out, "env.json"))
        self.conf(tasks=[task()])
        self.run_runner(self.args("--owner", "w1", "--", sys.executable, child), extra_env={"BOARD_ACTIVITY": "1"})
        self.assertEqual(self.read_json(os.path.join(out, "env.json")), [])

    def test_a_released_task_on_shutdown_is_published_as_ok(self):
        # the label function and the outcome mapping are read from the module: a shutdown release is not an error
        mod = test_run.load_runner_module()
        self.assertTrue(hasattr(mod.Runner, "activity"))
        with open(test_run.RUNNER) as f:
            src = f.read()
        self.assertIn('outcome="ok" if outcome in ("done", "released on shutdown") else "error"', src)


if __name__ == "__main__":
    unittest.main()
