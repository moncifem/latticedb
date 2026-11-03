"""Tests of the swarm's own checks (integrations/board/swarm), without a database.

The swarm is a soak test: its value is the invariant checker that replays the board's event log. That
checker must catch each kind of violation, so these tests feed it hand-written event logs, one fault
at a time. The expectations come from the board's rules (CONVENTIONS.md), not from the swarm's output.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import importlib.machinery
import importlib.util
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SWARM = os.path.join(os.path.dirname(HERE), "swarm")


def load():
    loader = importlib.machinery.SourceFileLoader("swarm_under_test", SWARM)
    spec = importlib.util.spec_from_loader("swarm_under_test", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def ev(eid, task, author, kind, body=""):
    return (eid, task, author, kind, body)


class Invariants(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = load()

    def check(self, events, blockers=None):
        return self.mod.check_events(events, blockers or {})

    def test_a_clean_history_has_no_violation(self):
        events = [
            ev(1, "a", "w1", "add"), ev(2, "a", "w1", "claim"), ev(3, "a", "w1", "note"),
            ev(4, "a", "w1", "fail"), ev(5, "a", "w2", "claim"), ev(6, "a", "w2", "done"),
            ev(7, "b", "w1", "claim"), ev(8, "b", "w3", "expired"), ev(9, "b", "w2", "claim"),
            ev(10, "b", "w2", "release"), ev(11, "b", "w1", "claim"), ev(12, "b", "w1", "done"),
            ev(13, "a", "w1", "fenced"),
        ]
        violations, counts = self.check(events)
        self.assertEqual(violations, [])
        self.assertEqual((counts["claim"], counts["done"], counts["expired"], counts["fenced"]), (5, 2, 1, 1))

    def test_done_twice_is_caught(self):
        events = [ev(1, "a", "w1", "claim"), ev(2, "a", "w1", "done"), ev(3, "a", "w1", "claim"), ev(4, "a", "w1", "done")]
        violations, _ = self.check(events)
        self.assertTrue(any("done twice" in v for v in violations), violations)

    def test_done_by_someone_who_does_not_hold_the_claim_is_caught(self):
        violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w2", "done")])
        self.assertTrue(any("marked done by w2" in v for v in violations), violations)

    def test_done_by_nobody_holding_is_caught(self):
        violations, _ = self.check([ev(1, "a", "w2", "done")])
        self.assertTrue(violations)

    def test_two_owners_at_once_is_caught(self):
        violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w2", "claim")])
        self.assertTrue(any("still holds it" in v for v in violations), violations)

    def test_a_claim_after_the_holder_gave_the_task_back_is_fine(self):
        for end in ("fail", "release", "expired"):
            violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w1", end), ev(3, "a", "w2", "claim")])
            self.assertEqual(violations, [], end)

    def test_fail_or_release_by_a_non_holder_is_caught(self):
        for kind in ("fail", "release"):
            violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w2", kind)])
            self.assertTrue(violations, kind)

    def test_a_note_from_someone_who_lost_the_claim_is_caught(self):
        violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w1", "expired"), ev(3, "a", "w1", "note")])
        self.assertTrue(any("note on a" in v for v in violations), violations)

    def test_an_expiry_of_a_lease_nobody_held_is_caught(self):
        violations, _ = self.check([ev(1, "a", "w3", "expired")])
        self.assertTrue(violations)

    def test_claiming_before_a_blocker_is_done_is_caught(self):
        blockers = {"b": ["a"]}
        violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "b", "w2", "claim"), ev(3, "a", "w1", "done")], blockers)
        self.assertTrue(any("before its blocker a" in v for v in violations), violations)
        violations, _ = self.check([ev(1, "a", "w1", "claim"), ev(2, "a", "w1", "done"), ev(3, "b", "w2", "claim")], blockers)
        self.assertEqual(violations, [])

    def test_events_of_tasks_the_swarm_did_not_create_are_ignored(self):
        events = self.mod.parse_events("1\ta\tw1\tclaim\t\n2\tx\tw9\tdone\t\n3\ta\tw1\tdone\tok\n", {"a"})
        self.assertEqual([e[1] for e in events], ["a", "a"])
        self.assertEqual(events[1][4], "ok")

    def test_events_are_replayed_in_id_order_whatever_the_output_order(self):
        events = self.mod.parse_events("2\ta\tw1\tdone\t\n1\ta\tw1\tclaim\t\n", {"a"})
        violations, _ = self.check(events)
        self.assertEqual(violations, [])


class ClosedTasks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = load()

    def test_a_claim_after_done_or_cancel_is_caught(self):
        for end in ("done", "cancel"):
            events = [ev(1, "a", "w1", "claim")]
            events.append(ev(2, "a", "w1", "done") if end == "done" else ev(2, "a", "w1", "cancel"))
            events.append(ev(3, "a", "w2", "claim"))
            violations, _ = self.mod.check_events(events, {})
            self.assertTrue(any("after it was done or cancelled" in v for v in violations), (end, violations))


class Stranded(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = load()

    def swarm(self, blockers):
        class A:
            seed = 1

        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        a = type("A", (), {"seed": 1, "workers": 1, "tasks": 1, "lease": 6, "fail_rate": 0, "zombie_rate": 0,
                           "cancel_rate": 0, "max_attempts": 3})()
        sw = self.mod.Swarm(a, d)
        sw.blockers = blockers
        return sw

    def test_open_tasks_behind_a_dead_or_cancelled_blocker_can_never_run(self):
        sw = self.swarm({"c": ["b"], "b": ["a"], "d": ["z"], "e": []})
        snap = {"a": {"state": "dead"}, "b": {"state": "open"}, "c": {"state": "open"},
                "d": {"state": "open"}, "z": {"state": "cancelled"}, "e": {"state": "open"}}
        self.assertEqual(sw.stranded(snap), {"b", "c", "d"})
        self.assertFalse(sw.finished(snap), "e can still run")
        snap["e"] = {"state": "done"}
        self.assertTrue(sw.finished(snap))

    def test_not_finished_while_something_runs_or_a_task_is_unseen(self):
        sw = self.swarm({})
        sw.created = {"a", "b"}
        self.assertFalse(sw.finished({"a": {"state": "done"}}))
        self.assertFalse(sw.finished({"a": {"state": "done"}, "b": {"state": "claimed"}}))
        self.assertTrue(sw.finished({"a": {"state": "done"}, "b": {"state": "dead"}}))


class Agent(unittest.TestCase):
    def run_agent(self, title, attempt, **env):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        body = os.path.join(d, "body.txt")
        result = os.path.join(d, "result.json")
        with open(body, "w") as f:
            f.write("x")
        e = {"PATH": os.environ.get("PATH", ""), "TASK_TITLE": title, "TASK_ATTEMPT": str(attempt),
             "TASK_BODY_PATH": body, "TASK_RESULT_PATH": result,
             "TASK_PARENT_PID": str(os.getpid()), "SWARM_SEED": "5", "SWARM_WORK_MAX": "0.01",
             "SWARM_HANG_SECONDS": "0.01"}
        e.update({k: str(v) for k, v in env.items()})
        p = subprocess.run([sys.executable, SWARM, "_agent"], env=e, capture_output=True, text=True, timeout=30)
        return p, result

    def test_a_clean_run_writes_a_done_result(self):
        p, result = self.run_agent("swarm5 task 01", 1)
        self.assertEqual(p.returncode, 0, p.stderr)
        with open(result) as f:
            self.assertIn('"status": "done"', f.read())

    def test_the_outcome_depends_on_seed_title_and_attempt_only(self):
        outcomes = []
        for _ in range(2):
            row = []
            for attempt in range(1, 7):
                p, _ = self.run_agent("swarm5 task 02", attempt, SWARM_FAIL_RATE=0.6)
                row.append(p.returncode if p.returncode >= 0 else None)
            outcomes.append(row)
        self.assertEqual(outcomes[0], outcomes[1])
        self.assertTrue({0, 1, 75} & set(outcomes[0]))

    def test_an_agent_whose_worker_is_gone_stops_and_never_signals_anyone(self):
        for parent in ("1", "999999", "0"):
            p, result = self.run_agent("swarm5 task 03", 1, TASK_PARENT_PID=parent, SWARM_ZOMBIE_RATE=1)
            self.assertEqual(p.returncode, 1, parent)
            self.assertFalse(os.path.exists(result))

    def test_the_zombie_signals_only_the_worker_named_in_task_parent_pid(self):
        # A stand-in worker that starts the agent and reports how it ended.
        code = ("import os, subprocess, sys\n"
                "env = dict(os.environ, TASK_PARENT_PID=str(os.getpid()))\n"
                "subprocess.run([sys.executable, %r, '_agent'], env=env)\n"
                "print('survived')\n" % SWARM)
        p = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=30,
                           env={"PATH": os.environ.get("PATH", ""), "TASK_TITLE": "swarm5 task 04", "TASK_ATTEMPT": "1",
                                "TASK_BODY_PATH": SWARM, "TASK_RESULT_PATH": os.devnull, "SWARM_SEED": "5",
                                "SWARM_ZOMBIE_RATE": "1"})
        self.assertEqual(p.returncode, -signal.SIGKILL, p.stdout + p.stderr)
        self.assertNotIn("survived", p.stdout)

    def test_exit_codes_for_failures_are_one_or_75(self):
        codes = set()
        for i in range(30):
            p, _ = self.run_agent("swarm5 task %02d" % i, 1, SWARM_FAIL_RATE=0.9)
            if p.returncode != 0:
                codes.add(p.returncode)
        self.assertTrue(codes <= {1, 75}, codes)
        self.assertTrue(codes, "a high fail rate must fail sometimes")


class Cli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="swarm-cli-")
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def fake_board(self, body):
        path = os.path.join(self.tmp, "board")
        with open(path, "w") as f:
            f.write("#!/bin/sh\n" + body)
        os.chmod(path, 0o755)
        return path

    def swarm(self, *args, board=None):
        env = {"PATH": os.environ.get("PATH", ""), "TMPDIR": self.tmp}
        if board:
            env["SWARM_BOARD"] = board
        return subprocess.run([sys.executable, SWARM, *args], env=env, capture_output=True, text=True, timeout=60)

    def test_bad_options_exit_2(self):
        for args in (["--tasks", "0"], ["--workers", "99"], ["--fail-rate", "1.5"], ["--fail-rate", "0.7", "--zombie-rate", "0.5"],
                     ["--lease", "1"]):
            p = self.swarm(*args)
            self.assertEqual(p.returncode, 2, args)

    def test_it_refuses_a_board_with_tasks_it_did_not_create(self):
        board = self.fake_board('case $1 in list) printf "id\\tstate\\tpriority\\towner\\ttitle\\n"; '
                                '[ "$2" = open ] && printf "tabc\\topen\\t0\\t\\tsomeone else\\n";; esac\nexit 0\n')
        p = self.swarm("--tasks", "2", "--workers", "1", board=board)
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertIn("not the swarm's", p.stderr)
        self.assertIn("tabc", p.stderr)

    def test_foreign_tasks_need_yes_as_well(self):
        p = self.swarm("--allow-foreign-tasks")
        self.assertEqual(p.returncode, 2)
        self.assertIn("--yes", p.stderr)

    def test_sigterm_stops_the_workers(self):
        pidfile = os.path.join(self.tmp, "worker.pid")
        counter = os.path.join(self.tmp, "n")
        board = self.fake_board(
            'case $1 in\n init) echo ready;;\n list) printf "id\\tstate\\tpriority\\towner\\ttitle\\n";;\n'
            ' add) n=$(cat "%s" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "%s"; echo "tfake$n";;\n'
            ' run) echo $$ > "%s"; exec sleep 300;;\nesac\nexit 0\n' % (counter, counter, pidfile))
        env = {"PATH": os.environ.get("PATH", ""), "TMPDIR": self.tmp, "SWARM_BOARD": board}
        p = subprocess.Popen([sys.executable, SWARM, "--workers", "1", "--tasks", "2", "--timeout", "120"], env=env,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: p.poll() is None and p.kill())
        end = time.time() + 30
        while not os.path.exists(pidfile) and time.time() < end:
            time.sleep(0.1)
        self.assertTrue(os.path.exists(pidfile), "the worker never started")
        time.sleep(0.5)
        with open(pidfile) as f:
            pid = int(f.read())
        p.send_signal(signal.SIGTERM)
        p.communicate(timeout=60)
        time.sleep(0.5)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_a_board_that_cannot_be_reached_is_a_setup_error_not_a_pass(self):
        board = self.fake_board('echo "board: database down" >&2\nexit 1\n')
        p = self.swarm("--tasks", "2", "--workers", "1", board=board)
        self.assertEqual(p.returncode, 2)
        self.assertIn("database down", p.stderr)


if __name__ == "__main__":
    unittest.main()
