"""`board run --after-cmd`: the hook board-bench uses to record an attempt under the task's fence.

The hook runs in the runner's own environment (the board credentials, which the child never has), after a
child that ended well and before the task is marked done, while the lease is still held. These tests use
the same fake board helper and small fake children as test_run.py.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import os
import sys
import textwrap
import unittest

from test_run import SECRET, RunnerCase, slurp, task


class AfterCmd(RunnerCase):
    def hook(self, body, name="hook.py"):
        out = self.sandbox("hook-out")
        return self.script(name, "import json, os, sys\nOUT = %r\n" % out + textwrap.dedent(body).lstrip("\n")), out

    def test_the_hook_runs_after_the_child_and_before_done_with_the_task_variables_and_the_credentials(self):
        self.conf(tasks=[task("t1", attempts=1)])
        child = self.script("ok.py", "import os\nopen(os.environ['TASK_RESULT_PATH'], 'w').write('{\"status\": \"done\", \"summary\": \"s\"}')\n")
        hook, out = self.hook("""
            import time
            json.dump({"env": dict(os.environ), "result_exists": os.path.exists(os.environ["TASK_RESULT_PATH"]),
                       "cwd": os.getcwd(), "t": time.time()}, open(OUT + "/hook.json", "w"))
        """)
        p = self.run_runner(self.args("--owner", "w1", "--after-cmd", "%s %s" % (sys.executable, hook), "--",
                                      sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        seen = self.read_json(os.path.join(out, "hook.json"))
        env = seen["env"]
        self.assertEqual((env["TASK_ID"], env["TASK_FENCE"], env["TASK_ATTEMPT"], env["BOARD_AUTHOR"]), ("t1", "101", "2", "w1"))
        self.assertEqual(env["PGPASSWORD"], SECRET)  # the hook is the runner's own code, with its credentials
        self.assertTrue(seen["result_exists"])  # the child's result file is there to read
        self.assertTrue(env["TASK_RESULT_PATH"].startswith(os.path.realpath(self.results)))
        self.assertEqual(self.calls("done")[0]["argv"][:4], ["done", "t1", "--fence", "101"])
        # the order of events: the hook ran before the board was told the task is done
        self.assertLess(seen["t"], self.calls("done")[0]["t"])

    def test_the_child_never_gets_what_the_hook_gets(self):
        self.conf(tasks=[task()])
        out = self.sandbox()
        child = self.script("dump.py", "import json, os\njson.dump(sorted(os.environ), open(%r, 'w'))\n" % os.path.join(out, "child.json"))
        hook, _ = self.hook("pass\n")
        p = self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        names = self.read_json(os.path.join(out, "child.json"))
        self.assertFalse([n for n in names if n.startswith(("PG", "BOARD_", "LATTICE_"))], names)

    def test_a_hook_that_exits_3_means_the_lease_is_lost_and_the_task_is_not_marked(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        hook, _ = self.hook("sys.exit(3)\n")
        p = self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        self.assertIn("lost at the after-cmd; the fence refused", p.out)
        self.assertEqual(self.calls("done"), [])
        self.assertEqual(self.calls("fail"), [])
        self.assertIn("lost=1", p.err)

    def test_a_hook_that_fails_fails_the_task_with_its_output(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        hook, _ = self.hook("print('database is down'); sys.exit(1)\n")
        p = self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        self.assertEqual(self.calls("done"), [])
        fail = self.calls("fail")[0]["argv"]
        self.assertIn("the after-cmd failed (exit 1): database is down", fail[-1])
        self.assertIn("failed", p.out)

    def test_a_hook_that_hangs_is_killed_and_fails_the_task(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        hook, _ = self.hook("import time\ntime.sleep(60)\n")
        p = self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--after-timeout", "1", "--", sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        self.assertIn("did not end in 1s", self.calls("fail")[0]["argv"][-1])

    def test_the_hook_is_not_run_for_a_child_that_failed_or_asked_to_retry(self):
        for code in (1, 75):
            self.conf(tasks=[task("t%d" % code)])
            child = self.script("bad%d.py" % code, "import sys\nsys.exit(%d)\n" % code)
            hook, out = self.hook("open(OUT + '/ran', 'w').write('x')\n", "hook%d.py" % code)
            self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
            self.assertFalse(os.path.exists(os.path.join(out, "ran")), code)

    def test_the_hook_is_not_run_for_a_child_whose_result_says_fail(self):
        self.conf(tasks=[task()])
        child = self.script("f.py", "import os\nopen(os.environ['TASK_RESULT_PATH'], 'w').write('{\"status\": \"fail\"}')\n")
        hook, out = self.hook("open(OUT + '/ran', 'w').write('x')\n")
        self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
        self.assertFalse(os.path.exists(os.path.join(out, "ran")))

    def test_no_hook_changes_nothing(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        p = self.run_runner(self.args("--", sys.executable, child))
        self.assertIn("done rc=0", p.out)

    def test_a_missing_hook_program_stops_the_runner_before_it_claims_anything(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        p = self.run_runner(self.args("--after-cmd", "/nonexistent/hook", "--", sys.executable, child))
        self.assertEqual(p.returncode, 2)
        self.assertIn("cannot find the after-cmd program", p.err)
        self.assertEqual(self.calls("next"), [])

    def test_option_checks(self):
        child = self.script("ok.py", "pass\n")
        for opts in (["--after-cmd", ""], ["--after-cmd", "'unclosed"], ["--after-cmd", "x", "--after-timeout", "0"],
                     ["--after-cmd", "x", "--after-timeout", "99999"]):
            p = self.run_runner(self.args(*opts, "--", sys.executable, child))
            self.assertEqual(p.returncode, 2, opts)

    def test_output_of_the_hook_is_redacted_before_it_reaches_the_board(self):
        self.conf(tasks=[task()])
        child = self.script("ok.py", "pass\n")
        hook, _ = self.hook("print('password is ' + os.environ['PGPASSWORD']); sys.exit(1)\n")
        self.run_runner(self.args("--after-cmd", "%s %s" % (sys.executable, hook), "--", sys.executable, child))
        text = self.calls("fail")[0]["argv"][-1]
        self.assertNotIn(SECRET, text)
        self.assertIn("[redacted]", text)


if __name__ == "__main__":
    unittest.main()
