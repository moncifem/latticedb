"""board-bench's commands against a fake board helper (fakebench.py): start, approve, cancel, record, report,
list, and the whole path with real `board run` workers: start -> approve -> workers -> report.

The helper's SQL is covered elsewhere (test_bench_sqlshape.py without a database, bench_devstack.py against
a real latticed). Here the question is whether the commands call the helper correctly, in the right order,
with the right fields, and say the right thing when it refuses.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

import benchbase
from benchbase import BENCH, BOARD_DIR, EXAMPLE_SUITE, REAL_BOARD_DIR
from benchlib import report

HERE = os.path.dirname(os.path.abspath(__file__))
FAKE = os.path.join(HERE, "fakebench.py")
RUNNER = os.path.join(BOARD_DIR, "board-run")


def slurp(path):
    with open(path) as f:
        return f.read()


class Bench(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bench-cli-")
        os.chmod(self.tmp, 0o700)
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.fb = os.path.join(self.tmp, "fb")
        os.mkdir(self.fb)
        self.board = os.path.join(self.tmp, "board")
        shutil.copy(FAKE, self.board)
        os.chmod(self.board, 0o755)
        self.suite_dir = os.path.join(self.tmp, "ex")
        shutil.copytree(os.path.dirname(EXAMPLE_SUITE), self.suite_dir)
        self.suite = os.path.join(self.suite_dir, "suite.json")
        self.env = {"PATH": os.environ["PATH"], "HOME": self.tmp, "FAKEBENCH_DIR": self.fb, "BOARD_BENCH_BOARD": self.board,
                    "BOARD_RUN_BOARD": self.board, "BOARD_AUTHOR": "bench-test", "BOARD_STATE_DIR": os.path.join(self.tmp, "st")}

    def bench(self, *args, env=None, timeout=120):
        e = dict(self.env)
        e.update(env or {})
        return subprocess.run([sys.executable, BENCH, *args], env=e, capture_output=True, text=True, timeout=timeout)

    def conf(self, **kw):
        with open(os.path.join(self.fb, "conf.json"), "w") as f:
            json.dump(kw, f)

    def state(self):
        return json.loads(slurp(os.path.join(self.fb, "state.json")))

    def calls(self):
        return [json.loads(x) for x in slurp(os.path.join(self.fb, "calls.jsonl")).splitlines()]

    def start(self, name="exp1", *extra):
        r = self.bench("start", self.suite, "--experiment", name, "--seed", "3", *extra)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r


class StartTests(Bench):
    def test_start_registers_the_experiment_its_arms_tasks_and_one_board_task_per_attempt(self):
        r = self.start()
        s = self.state()
        e = s["experiments"]["exp1"]
        self.assertEqual((e["suite"], e["seed"], e["repeats"], e["planned"]), ("example-fake", "3", "2", "16"))
        self.assertEqual(len(e["sha"]), 64)
        self.assertEqual(sorted(a["arm"] for a in s["arms"]), ["baseline", "candidate"])
        self.assertEqual(sorted(t["task"] for t in s["bench_tasks"]), ["calc-add", "calc-flaky", "calc-infra", "greet-hello"])
        self.assertEqual(len(s["tasks"]), 16)
        self.assertEqual(len(s["attempts"]), 16)
        self.assertIn("16 attempts planned (seed 3", r.stdout)
        self.assertIn("a 10-point difference", r.stdout)

    def test_every_board_task_waits_for_approval_and_nothing_is_approved_unless_asked(self):
        self.start()
        self.assertEqual({t["state"] for t in self.state()["tasks"].values()}, {"awaiting_approval"})
        self.assertIn("board-bench approve exp1", self.bench("list").stdout + "board-bench approve exp1")
        self.assertFalse([c for c in self.calls() if c["argv"][0] == "approve"])

    def test_approve_flag_opens_them_at_once(self):
        r = self.start("exp2", "--approve")
        self.assertEqual({t["state"] for t in self.state()["tasks"].values()}, {"open"})
        self.assertIn("the attempts are approved", r.stdout)

    def test_the_task_body_carries_ids_and_the_suite_sha_only(self):
        self.start()
        sha = self.state()["experiments"]["exp1"]["sha"]
        for t in self.state()["tasks"].values():
            body = json.loads(t["body"])
            self.assertEqual(set(body), {"b", "exp", "arm", "task", "rep", "ord", "suite", "sha"})
            self.assertEqual((body["exp"], body["sha"]), ("exp1", sha))
            self.assertEqual(body["suite"], os.path.realpath(self.suite))
            self.assertNotIn("prompt", t["body"])
            self.assertTrue(t["title"].startswith("bench exp1 "))

    def test_creation_order_is_the_matrix_order_and_arms_alternate_inside_cells(self):
        self.start()
        order = [json.loads(self.state()["tasks"][tid]["body"]) for tid in self.state()["order"]]
        self.assertEqual([b["ord"] for b in order], list(range(16)))
        for i in range(0, 16, 2):
            self.assertEqual((order[i]["task"], order[i]["rep"]), (order[i + 1]["task"], order[i + 1]["rep"]))
            self.assertNotEqual(order[i]["arm"], order[i + 1]["arm"])
        self.assertNotEqual([b["task"] for b in order[::2]], sorted(b["task"] for b in order[::2]))

    def test_the_same_seed_gives_the_same_order_and_no_seed_picks_and_records_one(self):
        self.start("a")
        self.start("b")
        orders = [[json.loads(self.state()["tasks"][t]["body"])["ord"] for t in self.state()["order"]][i:i + 16] for i in (0, 16)]
        arms = [[(json.loads(self.state()["tasks"][t]["body"])["arm"], json.loads(self.state()["tasks"][t]["body"])["task"])
                 for t in self.state()["order"]][i:i + 16] for i in (0, 16)]
        self.assertEqual(arms[0], arms[1])
        self.assertEqual(orders[0], orders[1])
        r = self.bench("start", self.suite, "--experiment", "c")
        self.assertEqual(r.returncode, 0)
        self.assertRegex(self.state()["experiments"]["c"]["seed"], r"^\d+$")
        self.assertIn("seed %s" % self.state()["experiments"]["c"]["seed"], r.stdout)

    def test_a_taken_name_is_refused_and_nothing_else_is_made(self):
        self.start()
        before = len(self.state()["tasks"])
        r = self.bench("start", self.suite, "--experiment", "exp1", "--seed", "3")
        self.assertEqual(r.returncode, 1)
        self.assertIn("already exists", r.stderr)
        self.assertEqual(len(self.state()["tasks"]), before)

    def test_a_failure_halfway_says_how_to_clean_up(self):
        self.conf(fail={"bench-plan": 1})
        r = self.bench("start", self.suite, "--experiment", "half", "--seed", "3")
        self.assertEqual(r.returncode, 1)
        self.assertIn("may be partly made", r.stderr)
        self.assertIn("board-bench cancel half", r.stderr)

    def test_bad_names_and_bad_suites_are_refused_before_the_board_is_touched(self):
        for name in ("has space", "a/b", "x" * 49):
            r = self.bench("start", self.suite, "--experiment", name)
            self.assertEqual(r.returncode, 2, name)
        with open(os.path.join(self.suite_dir, "fixtures", "calc", "calc.py"), "a") as f:
            f.write("# edited\n")
        r = self.bench("start", self.suite, "--experiment", "x")
        self.assertEqual(r.returncode, 2)
        self.assertIn("hashes to", r.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.fb, "calls.jsonl")))

    def test_the_board_helper_is_the_only_way_in(self):
        self.start()
        verbs = {c["argv"][0] for c in self.calls()}
        self.assertEqual(verbs, {"bench-experiment", "bench-arm", "bench-task", "add", "bench-plan"})
        self.assertEqual({c["author"] for c in self.calls()}, {"bench-test"})


class ApproveCancelTests(Bench):
    def test_approve_opens_only_attempts_that_are_not_recorded(self):
        self.start()
        first = self.state()["order"][0]
        s = self.state()
        s["attempts"][first]["outcome"] = "pass"
        with open(os.path.join(self.fb, "state.json"), "w") as f:
            json.dump(s, f)
        r = self.bench("approve", "exp1")
        self.assertIn("approved 15 of 15", r.stdout)
        self.assertEqual(self.state()["tasks"][first]["state"], "awaiting_approval")

    def test_cancel(self):
        self.start()
        r = self.bench("cancel", "exp1")
        self.assertIn("cancelled 16 of 16", r.stdout)
        self.assertEqual({t["state"] for t in self.state()["tasks"].values()}, {"cancelled"})

    def test_unknown_experiment(self):
        self.start()
        r = self.bench("approve", "nope")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no such experiment", r.stderr)
        self.assertEqual(self.bench("approve", "../x").returncode, 2)


class RecordTests(Bench):
    def setUp(self):
        super().setUp()
        self.start()
        self.tid = self.state()["order"][0]
        self.worker_env = {"BOARD_AUTHOR": "w1"}
        r = subprocess.run([self.board, "claim-for-test", self.tid, "--fence", "4242"], env=dict(self.env, **self.worker_env),
                           capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        a = self.state()["attempts"][self.tid]
        self.ident = {"exp": a["exp"], "arm": a["arm"], "task": a["task"]}

    def result_file(self, bench):
        p = os.path.join(self.tmp, "result.json")
        with open(p, "w") as f:
            json.dump({"status": "done", "summary": "x", "bench": dict(self.ident, **bench)}, f)
        return p

    def record(self, *args, fence="4242", env=None):
        e = dict(self.worker_env)
        e.update(env or {})
        who = ["--experiment", self.ident["exp"], "--arm", self.ident["arm"], "--bench-task", self.ident["task"]] if "--outcome" in args else []
        return self.bench("record", "--task", self.tid, "--fence", fence, *who, *args, env=e)

    def test_a_result_about_another_attempt_is_refused_and_nothing_is_written(self):
        other = dict(self.ident, arm="candidate" if self.ident["arm"] == "baseline" else "baseline")
        p = os.path.join(self.tmp, "other.json")
        with open(p, "w") as f:
            json.dump({"bench": dict(other, outcome="pass")}, f)
        r = self.record("--from-result", p)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("nothing written", r.stderr)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "planned")
        # the fake helper applies the same rule, so a client that skipped the check would still be refused
        for key in ("exp", "task"):
            q = os.path.join(self.tmp, "other2.json")
            with open(q, "w") as f:
                json.dump({"bench": dict(self.ident, **{key: "zzz"}, outcome="pass")}, f)
            self.assertNotEqual(self.record("--from-result", q).returncode, 0, key)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "planned")

    def test_a_result_that_does_not_say_which_attempt_it_is_about_is_refused(self):
        p = os.path.join(self.tmp, "noid.json")
        with open(p, "w") as f:
            json.dump({"bench": {"outcome": "pass"}}, f)
        self.assertEqual(self.record("--from-result", p).returncode, 2)
        self.assertEqual(self.bench("record", "--task", self.tid, "--fence", "4242", "--outcome", "pass", env=self.worker_env).returncode, 2)

    def test_a_holder_records_from_the_result_file_with_every_field(self):
        p = self.result_file({"outcome": "pass", "flaky": True, "tampered": 2, "duration_ms": 1234, "cost_usd": 0.0421,
                              "tokens": 77, "grader_exit": 0, "grader_runs": "pfp", "summary": "grader runs pfp; OK"})
        r = self.record("--from-result", p)
        self.assertEqual(r.returncode, 0, r.stderr)
        a = self.state()["attempts"][self.tid]
        self.assertEqual((a["outcome"], a["flaky"], a["tampered"], a["duration_ms"], a["cost"], a["tokens"], a["gexit"], a["runs"]),
                         ("pass", 1, 2, "1234", "0.042100", "77", "0", "pfp"))

    def test_unknown_values_are_not_sent(self):
        p = self.result_file({"outcome": "fail", "flaky": False, "tampered": 0, "duration_ms": None, "cost_usd": None,
                              "tokens": None, "grader_exit": 1, "grader_runs": "f", "summary": ""})
        self.assertEqual(self.record("--from-result", p).returncode, 0)
        sent = [c for c in self.calls() if c["argv"][0] == "bench-record"][0]["argv"]
        for opt in ("--cost", "--tokens", "--duration-ms", "--summary"):
            self.assertNotIn(opt, sent)
        a = self.state()["attempts"][self.tid]
        self.assertEqual((a["cost"], a["tokens"], a["duration_ms"]), ("", "", ""))

    def test_a_worker_that_lost_the_lease_cannot_record(self):
        p = self.result_file({"outcome": "pass"})
        r = self.record("--from-result", p, fence="4241")  # an older claim's fence
        self.assertEqual(r.returncode, 3)
        self.assertIn("lease lost", r.stderr)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "planned")
        r = self.record("--from-result", p, env={"BOARD_AUTHOR": "intruder"})
        self.assertEqual(r.returncode, 3)

    def test_recording_twice_changes_nothing(self):
        self.assertEqual(self.record("--outcome", "pass").returncode, 0)
        r = self.record("--outcome", "fail")
        self.assertEqual(r.returncode, 0)
        self.assertIn("already recorded", r.stdout)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "pass")

    def test_standalone_flags(self):
        r = self.record("--outcome", "error", "--summary", "disk full", "--tampered", "1")
        self.assertEqual(r.returncode, 0, r.stderr)
        a = self.state()["attempts"][self.tid]
        self.assertEqual((a["outcome"], a["summary"], a["tampered"]), ("error", "disk full", 1))

    def test_no_fence_no_record(self):
        r = self.bench("record", "--task", self.tid, "--outcome", "pass", env=self.worker_env)
        self.assertEqual(r.returncode, 2)
        self.assertIn("needs a task id and a fence", r.stderr)
        self.assertEqual(self.record("--outcome", "pass", fence="abc").returncode, 2)

    def test_the_runner_variables_are_enough(self):
        p = self.result_file({"outcome": "pass"})
        r = self.bench("record", env={"BOARD_AUTHOR": "w1", "TASK_ID": self.tid, "TASK_FENCE": "4242", "TASK_RESULT_PATH": p})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "pass")

    def test_a_bad_result_file_is_refused(self):
        for doc in ("not json", "{}", '{"bench": {"outcome": "great"}}', '{"bench": {"outcome": "pass", "tokens": -1}}',
                    '{"bench": {"outcome": "pass", "cost_usd": 1e99}}', '{"bench": {"outcome": "pass", "summary": 5}}',
                    '{"bench": {"outcome": "pass", "duration_ms": true}}'):
            p = os.path.join(self.tmp, "bad.json")
            with open(p, "w") as f:
                f.write(doc)
            self.assertEqual(self.record("--from-result", p).returncode, 2, doc)
        self.assertEqual(self.record("--from-result", os.path.join(self.tmp, "missing")).returncode, 2)
        self.assertEqual(self.state()["attempts"][self.tid]["outcome"], "planned")


class ReportCommandTests(Bench):
    def test_report_text_and_json_agree_and_follow_the_rules(self):
        self.start()
        s = self.state()
        # record by hand: baseline passes calc-add twice, fails greet twice; candidate passes both tasks twice
        for tid in s["order"]:
            b = json.loads(s["tasks"][tid]["body"])
            a = s["attempts"][tid]
            if b["task"] in ("calc-add", "greet-hello"):
                a["outcome"] = "pass" if (b["arm"] == "candidate" or b["task"] == "calc-add") else "fail"
            elif b["task"] == "calc-infra":
                a["outcome"] = "error"
                a["summary"] = "the grader could not run"
        with open(os.path.join(self.fb, "state.json"), "w") as f:
            json.dump(s, f)
        r = self.bench("report", "exp1", "--json")
        self.assertEqual(r.returncode, 0, r.stderr)
        rep = json.loads(r.stdout)
        base, cand = rep["arms"]
        self.assertEqual((base["arm"], base["attempts"], base["errors"], base["pending"]), ("baseline", 4, 2, 2))
        self.assertAlmostEqual(base["pass_at_1"]["value"], 0.5)      # (1 + 0) / 2 tasks
        self.assertAlmostEqual(cand["pass_at_1"]["value"], 1.0)
        c = rep["comparisons"][0]
        self.assertEqual(c["table"], {"both": 1, "a_only": 0, "b_only": 1, "neither": 0})
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        text = self.bench("report", "exp1").stdout
        self.assertIn("VERDICT: Inconclusive", text)
        self.assertIn("pass@1   50% (95% CI", text)
        self.assertIn("harness errors (not counted against any arm)", text)
        self.assertIn("4 attempts not recorded yet", text)

    def test_report_options(self):
        self.start()
        self.assertEqual(self.bench("report", "exp1", "--rule", "any").returncode, 0)
        self.assertEqual(self.bench("report", "exp1", "--min-tasks", "0").returncode, 2)
        self.assertEqual(self.bench("report", "nope").returncode, 1)

    def test_list(self):
        self.assertIn("no experiments", self.bench("list").stdout)
        self.start()
        self.assertIn("exp1", self.bench("list").stdout)


class EndToEndTests(Bench):
    """The real board-run workers, the real attempt command and the real record command, against the fake
    helper. The expected numbers are worked out by hand from the example suite: the baseline arm leaves the
    greeting bug alone; calc-flaky has a grader that disagrees with itself; calc-infra's grader reports an
    infrastructure error."""

    def run_workers(self, n_workers=2):
        runs = os.path.join(self.tmp, "runs")
        os.makedirs(runs, mode=0o700, exist_ok=True)
        procs = []
        for i in range(n_workers):
            cmd = [sys.executable, RUNNER, "--owner", "w%d" % i, "--poll", "0.05", "--lease", "30", "--task-timeout", "120",
                   "--result-dir", runs, "--after-cmd", "%s %s record" % (sys.executable, BENCH), "--",
                   sys.executable, BENCH, "attempt", "--suite-dir", self.suite_dir, "--work-dir", os.path.join(self.tmp, "work")]
            procs.append(subprocess.Popen(cmd, env=dict(self.env, BOARD_STATE_DIR=os.path.join(self.tmp, "st%d" % i)),
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
        outs = [p.communicate(timeout=300) for p in procs]
        for p, (out, err) in zip(procs, outs):
            self.assertEqual(p.returncode, 0, err)
        return outs

    def test_two_arms_through_the_whole_path(self):
        self.start()
        self.assertIn("approved 16 of 16", self.bench("approve", "exp1").stdout)
        self.run_workers(2)
        s = self.state()
        self.assertEqual({t["state"] for t in s["tasks"].values()}, {"done"})
        self.assertEqual(sorted(a["outcome"] for a in s["attempts"].values()),
                         ["error"] * 4 + ["fail"] * 2 + ["pass"] * 10)
        rep = json.loads(self.bench("report", "exp1", "--json").stdout)
        base, cand = rep["arms"]
        # baseline: calc-add 2/2, greet-hello 0/2, calc-flaky 2/2 (calc-infra: 2 errors, left out)
        self.assertEqual((base["tasks"], base["attempts"], base["passes"], base["errors"], base["flaky"]), (3, 6, 4, 2, 2))
        self.assertAlmostEqual(base["pass_at_1"]["value"], 2 / 3)
        self.assertAlmostEqual(base["pass_at_k"]["value"], 2 / 3)    # k = 2: 1, 0, 1
        self.assertAlmostEqual(base["pass_pow_k"]["value"], 2 / 3)
        self.assertEqual((cand["passes"], cand["attempts"], cand["flaky"]), (6, 6, 2))
        self.assertAlmostEqual(cand["pass_at_1"]["value"], 1.0)
        self.assertEqual(cand["cost"]["n"], 6)                     # the fake agent reports 0.01 per attempt
        self.assertAlmostEqual(cand["cost"]["total"], 0.06)
        c = rep["comparisons"][0]
        self.assertEqual((c["paired_tasks"], c["table"]), (3, {"both": 2, "a_only": 0, "b_only": 1, "neither": 0}))
        self.assertAlmostEqual(c["mcnemar_p"], 1.0)                # one discordant task: 2 * 1/2
        self.assertAlmostEqual(c["diff"]["mean"], 1 / 3)
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertEqual(len(rep["errors"]), 4)
        self.assertTrue(all("could not run" in e["summary"] for e in rep["errors"]))
        # the workers' calls never carried a credential variable to the attempt command's own children
        self.assertEqual(rep["pending"], 0)

    def test_a_record_that_fails_makes_the_task_fail_and_retry_not_vanish(self):
        self.conf(flaky_record=1)
        self.start("exp3")
        self.bench("approve", "exp3")
        # one worker, retry delay 0: the failed task goes back to open and is run again
        runs = os.path.join(self.tmp, "runs")
        os.makedirs(runs, mode=0o700)
        r = subprocess.run([sys.executable, RUNNER, "--owner", "w0", "--poll", "0.05", "--lease", "30", "--retry-base", "0",
                            "--result-dir", runs, "--after-cmd", "%s %s record" % (sys.executable, BENCH), "--",
                            sys.executable, BENCH, "attempt", "--suite-dir", self.suite_dir, "--work-dir", os.path.join(self.tmp, "work")],
                           env=self.env, capture_output=True, text=True, timeout=300)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("attempt=1 failed", r.stdout)
        s = self.state()
        self.assertEqual({a["outcome"] == "planned" for a in s["attempts"].values()}, {False})
        self.assertEqual({t["state"] for t in s["tasks"].values()}, {"done"})


if __name__ == "__main__":
    unittest.main()
