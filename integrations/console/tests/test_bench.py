"""The Experiments screen: the read-only SQL, the data behind it, the API and the page script.

The numbers are computed by integrations/board/benchlib (the code `board-bench report` uses); these tests
check that the console feeds it the right rows, keeps the honest wording (every rate with its n and
interval, "inconclusive" as a result), and stays read-only and plain text.
"""

import json
import os
import re
import stat
import sys
import tempfile
import unittest

import helpers  # noqa: F401
from helpers import STATIC, FakeSource

from console import bench, demo, source
from console.demo import DemoSource, generate_bench
from console.service import BoardService
from console.source import PsqlSource, SourceError, build_select

import test_server
from test_static import FORBIDDEN_JS, findings, read, strip_comments

FAKE = r"""#!/usr/bin/env python3
import json, os, sys
sql = sys.stdin.read().strip()
d = os.environ["FAKE_DIR"]
with open(os.path.join(d, "sql.log"), "a") as f:
    f.write(sql + "\n")
mode = os.environ.get("FAKE_MODE", "out")
if mode == "nobench":
    sys.stderr.write('ERROR:  relation "bench_experiments" does not exist\n')
    sys.exit(2)
answers = json.load(open(os.path.join(d, "answers.json")))
for key, rows in answers.items():
    if key in sql:
        sys.stdout.write("".join("\0".join(r) + "\0" for r in rows))
        sys.exit(0)
sys.exit(0)
"""

EXP_ROW = ["e1", "suite1", "a" * 64, "7", "2", "4", "board-bench/1", "bob", "1700000000"]
ARM_ROWS = [["base", "cmd", "b" * 64], ["cand", "cmd", "c" * 64]]
TASK_ROWS = [["t1", "d" * 64], ["t2", "e" * 64]]


def attempt_row(tid, arm, task, rep, order, outcome, flaky="0", tampered="0", dur="", cost="", tokens="", gexit="", runs="", summary=""):
    return [tid, arm, task, str(rep), str(order), outcome, flaky, tampered, dur, cost, tokens, gexit, runs, "board-bench/1", "1700000100", summary]


class SqlTests(unittest.TestCase):
    OK = "SELECT board_task_id, arm FROM bench_attempts WHERE experiment_id = 'e-1_x' ORDER BY order_idx LIMIT 20001;"

    def test_the_bench_statements_pass_the_whitelist_and_nothing_near_them_does(self):
        self.assertTrue(source._SQL_ALLOWED.fullmatch(self.OK))
        self.assertTrue(source._SQL_ALLOWED.fullmatch("SELECT id FROM bench_experiments ORDER BY created_at DESC LIMIT 200;"))
        for bad in (self.OK + "\n", self.OK.replace("'e-1_x'", "'e'; DROP TABLE tasks; --'"),
                    self.OK.replace("'e-1_x'", "'" + "x" * 49 + "'"), self.OK.replace("bench_attempts", "pg_shadow"),
                    self.OK.replace("bench_attempts", "bench_attempts, tasks"), self.OK.replace("= 'e-1_x'", "= 'a' OR 1 = 1"),
                    self.OK.replace("WHERE experiment_id = 'e-1_x' ", ""), self.OK + " DELETE FROM bench_attempts;",
                    self.OK.replace("SELECT", "DELETE"), self.OK.replace("'e-1_x'", "\"e\"")):
            if bad == self.OK.replace("WHERE experiment_id = 'e-1_x' ", ""):
                self.assertTrue(source._SQL_ALLOWED.fullmatch(bad))  # the unfiltered list is allowed
                continue
            self.assertFalse(source._SQL_ALLOWED.fullmatch(bad), bad)

    def test_build_select_checks_the_experiment_name_again(self):
        sql = build_select(("arm",), "bench_arms", "arm", False, 5, experiment="exp-1")
        self.assertEqual(sql, "SELECT arm FROM bench_arms WHERE experiment_id = 'exp-1' ORDER BY arm LIMIT 5;")
        for bad in ("a'b", "a b", "", "x" * 49, "e';--"):
            with self.assertRaises(ValueError, msg=bad):
                build_select(("arm",), "bench_arms", "arm", False, 5, experiment=bad)

    def test_bench_data_refuses_a_bad_name_before_any_query(self):
        s = PsqlSource("", psql="/nonexistent/psql")
        with self.assertRaises(ValueError):
            s.bench_data("a'; DROP TABLE tasks")


class PsqlBenchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = self.tmp.name
        self.psql = os.path.join(self.dir, "psql")
        with open(self.psql, "w") as f:
            f.write(FAKE.replace("#!/usr/bin/env python3", "#!" + sys.executable))
        os.chmod(self.psql, os.stat(self.psql).st_mode | stat.S_IXUSR)

    def src(self, answers=None, mode="out"):
        with open(os.path.join(self.dir, "answers.json"), "w") as f:
            json.dump(answers or {}, f)
        env = {"FAKE_DIR": self.dir, "FAKE_MODE": mode, "PATH": os.environ.get("PATH", ""), "PGPASSWORD": "pw-9"}
        return PsqlSource("postgresql://dev@127.0.0.1:5999/lattice", psql=self.psql, env=env)

    def sql_log(self):
        with open(os.path.join(self.dir, "sql.log")) as f:
            return f.read().splitlines()

    def test_the_experiment_list_parses_every_column(self):
        rows, ok = self.src({"FROM bench_experiments": [EXP_ROW]}).bench_experiments()
        self.assertTrue(ok)
        self.assertEqual(rows[0], {"id": "e1", "suite": "suite1", "suite_sha": "a" * 64, "seed": 7, "repeats": 2, "planned": 4,
                                   "harness_version": "board-bench/1", "created_by": "bob", "created_at": 1700000000})
        self.assertEqual(self.sql_log()[0],
                         "SELECT id, suite, suite_sha, seed, repeats, planned, harness_version, created_by, created_at "
                         "FROM bench_experiments ORDER BY created_at DESC LIMIT 200;")

    def test_a_board_without_the_tables_is_an_empty_answer_not_an_error(self):
        self.assertEqual(self.src(mode="nobench").bench_experiments(), ([], False))

    def test_other_failures_are_errors(self):
        s = self.src(mode="nobench")
        s.psql = "/nonexistent/psql"
        with self.assertRaises(SourceError):
            s.bench_experiments()

    def test_one_experiment_is_read_with_four_fixed_selects(self):
        answers = {
            "FROM bench_experiments": [EXP_ROW],
            "FROM bench_arms": ARM_ROWS,
            "FROM bench_tasks": TASK_ROWS,
            "FROM bench_attempts": [
                attempt_row("t1", "base", "t1", 0, 0, "pass", flaky="1", tampered="2", dur="1500", cost="0.0421", tokens="", gexit="0", runs="pfp", summary="ok"),
                attempt_row("t2", "cand", "t1", 0, 1, "planned"),
            ]}
        d = self.src(answers).bench_data("e1")
        self.assertEqual(d["experiment"]["seed"], 7)
        self.assertEqual([a["arm"] for a in d["arms"]], ["base", "cand"])
        self.assertEqual([t["task"] for t in d["tasks"]], ["t1", "t2"])
        a, p = d["attempts"]
        self.assertEqual((a["outcome"], a["flaky"], a["tampered"], a["duration_ms"], a["cost_usd"], a["tokens"], a["grader_exit"]),
                         ("pass", True, 2, 1500, 0.0421, None, 0))
        self.assertEqual((p["outcome"], p["duration_ms"], p["cost_usd"], p["grader_exit"]), ("planned", None, None, None))
        self.assertEqual(p["repeat"], 0)
        log = self.sql_log()
        self.assertTrue(all(source._SQL_ALLOWED.fullmatch(x) for x in log), log)
        self.assertEqual(sum("WHERE experiment_id = 'e1'" in x for x in log), 3)

    def test_an_unknown_experiment_is_none(self):
        self.assertIsNone(self.src({"FROM bench_experiments": [EXP_ROW]}).bench_data("other"))

    def test_the_attempt_limit_is_marked_as_truncated(self):
        rows = [attempt_row("t%d" % i, "base", "t1", 0, i, "pass") for i in range(source.BENCH_ATTEMPT_LIMIT + 1)]
        d = self.src({"FROM bench_experiments": [EXP_ROW], "FROM bench_arms": ARM_ROWS, "FROM bench_tasks": TASK_ROWS,
                      "FROM bench_attempts": rows}).bench_data("e1")
        self.assertTrue(d["truncated"])
        self.assertEqual(len(d["attempts"]), source.BENCH_ATTEMPT_LIMIT)

    def test_a_bad_number_is_an_error_not_a_guess(self):
        bad = attempt_row("t1", "base", "t1", 0, 0, "pass", dur="soon")
        s = self.src({"FROM bench_experiments": [EXP_ROW], "FROM bench_arms": [], "FROM bench_tasks": [], "FROM bench_attempts": [bad]})
        with self.assertRaises(SourceError):
            s.bench_data("e1")
        bad = attempt_row("t1", "base", "t1", 0, 0, "pass", cost="lots")
        s = self.src({"FROM bench_experiments": [EXP_ROW], "FROM bench_arms": [], "FROM bench_tasks": [], "FROM bench_attempts": [bad]})
        with self.assertRaises(SourceError):
            s.bench_data("e1")


FLAKY = r"""#!PY
import os, sys
d = os.environ["FAKE_DIR"]
sys.stdin.read()
n = 0
try:
    n = int(open(os.path.join(d, "n")).read())
except OSError:
    pass
open(os.path.join(d, "n"), "w").write(str(n + 1))
msg = open(os.path.join(d, "msg")).read()
fail_first = int(open(os.path.join(d, "fail_first")).read())
if n < fail_first:
    sys.stderr.write(msg + "\n")
    sys.exit(2)
sys.stdout.write("1700000123\0")
"""

DROPPED = 'psql: error: connection to server at "127.0.0.1", port 5432 failed: server closed the connection unexpectedly'


class ConnectRetryTests(unittest.TestCase):
    """The alpha server drops a share of new connections (issue 867). The console only sends SELECT statements, so it asks
    again after a connect failure, and only then."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = self.tmp.name
        self.psql = os.path.join(self.dir, "psql")
        with open(self.psql, "w") as f:
            f.write(FLAKY.replace("#!PY", "#!" + sys.executable))
        os.chmod(self.psql, os.stat(self.psql).st_mode | stat.S_IXUSR)

    def src(self, msg, fail_first):
        for name, val in (("msg", msg), ("fail_first", str(fail_first))):
            with open(os.path.join(self.dir, name), "w") as f:
                f.write(val)
        if os.path.exists(os.path.join(self.dir, "n")):
            os.remove(os.path.join(self.dir, "n"))
        return PsqlSource("postgresql://dev@127.0.0.1:5999/x", psql=self.psql,
                          env={"FAKE_DIR": self.dir, "PATH": os.environ.get("PATH", "")})

    def tries(self):
        with open(os.path.join(self.dir, "n")) as f:
            return int(f.read())

    def test_a_dropped_connection_is_asked_again(self):
        s = self.src(DROPPED, 3)
        self.assertEqual(s.now(), 1700000123)
        self.assertEqual(s.clock_source, "database")
        self.assertEqual(self.tries(), 4)

    def test_the_retries_are_bounded(self):
        s = self.src(DROPPED, 99)
        s.now()
        self.assertEqual(s.clock_source, "client")
        self.assertEqual(self.tries(), source.CONNECT_TRIES)

    def test_a_wrong_password_a_tls_error_or_a_sql_error_is_never_retried(self):
        for msg in ('psql: error: connection to server at "h", port 1 failed: FATAL:  password authentication failed for user "x"',
                    'psql: error: connection to server at "h", port 1 failed: SSL error: certificate verify failed',
                    'psql: error: connection to server at "h", port 1 failed: fe_sendauth: no password supplied',
                    "ERROR:  relation \"task_leases\" does not exist"):
            s = self.src(msg, 99)
            s.now()
            self.assertEqual(self.tries(), 1, msg)

    def test_classifier(self):
        self.assertTrue(source.transient_connect_error(DROPPED))
        self.assertTrue(source.transient_connect_error('psql: error: connection to server at "h", port 1 failed: Connection refused'))
        self.assertTrue(source.transient_connect_error(
            'psql: error: connection to server at "h", port 1 failed: server sent an error response during SSL exchange'))
        for text in ("server sent an error response during SSL exchange",
                     "FATAL:  too many new connections, retry shortly",
                     "FATAL:  sorry, too many clients already"):
            self.assertTrue(source.transient_connect_error(
                'psql: error: connection to server at "h", port 1 failed: ' + text), text)
        self.assertFalse(source.transient_connect_error(
            'psql: error: connection to server at "h", port 1 failed: FATAL:  password authentication failed'))
        self.assertFalse(source.transient_connect_error("psql: error: could not translate host name"))
        self.assertFalse(source.transient_connect_error(""))

    def test_messages_are_asked_for_in_english_even_when_lc_all_is_set(self):
        s = PsqlSource("postgresql://dev@127.0.0.1:5999/x", psql=self.psql,
                       env={"FAKE_DIR": self.dir, "PATH": os.environ.get("PATH", ""), "LC_ALL": "fr_FR.UTF-8"})
        self.assertNotIn("LC_ALL", s.env)
        self.assertEqual(s.env["LC_MESSAGES"], "C")


class DemoBenchTests(unittest.TestCase):
    def setUp(self):
        self.data = generate_bench(7)

    def report(self, name):
        return bench._report.build_report(self.data[name])

    def test_the_demo_is_deterministic(self):
        self.assertEqual(json.dumps(generate_bench(7), sort_keys=True), json.dumps(generate_bench(7), sort_keys=True))
        self.assertNotEqual(json.dumps(generate_bench(8), sort_keys=True), json.dumps(generate_bench(7), sort_keys=True))

    def test_the_candidate_looks_better_on_a_few_tasks_and_the_report_says_inconclusive(self):
        rep = self.report("demo-small-lift")
        c = rep["comparisons"][0]
        self.assertGreater(c["diff"]["mean"], 0.1)  # B looks better
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertLess(c["paired_tasks"], rep["min_tasks"])

    def test_the_large_gap_on_forty_tasks_is_a_clear_difference(self):
        rep = self.report("demo-clear-gap")
        c = rep["comparisons"][0]
        self.assertEqual(c["verdict"]["label"], "b_better")
        self.assertGreater(c["diff"]["lo"], 0)
        self.assertGreaterEqual(c["paired_tasks"], rep["min_tasks"])

    def test_the_clear_gap_has_flaky_rows_harness_errors_timeouts_tampering_and_pending_attempts(self):
        rep = self.report("demo-clear-gap")
        self.assertGreater(sum(a["flaky"] for a in rep["arms"]), 0)
        self.assertGreater(len(rep["errors"]), 0)
        self.assertGreater(sum(a["timeouts"] for a in rep["arms"]), 0)
        self.assertGreater(sum(a["tampered"] for a in rep["arms"]), 0)
        self.assertGreater(rep["pending"], 0)
        self.assertTrue(any(a["cost"]["n"] for a in rep["arms"]))
        self.assertTrue(any(a["cost"]["n"] == 0 for a in rep["arms"]))  # one arm reports no cost: shown as not reported
        for e in rep["errors"]:
            self.assertRegex(e["summary"], r"digest|could not run")

    def test_errors_are_in_no_rate(self):
        rep = self.report("demo-clear-gap")
        for arm in rep["arms"]:
            rows = [a for a in self.data["demo-clear-gap"]["attempts"] if a["arm"] == arm["arm"]]
            counted = [a for a in rows if a["outcome"] in ("pass", "fail", "timeout")]
            self.assertEqual(arm["attempts"], len(counted))

    def test_an_experiment_nobody_has_recorded_shows_no_result_not_zero(self):
        rep = self.report("demo-just-started")
        for arm in rep["arms"]:
            self.assertIsNone(arm["pass_at_1"]["value"])
            self.assertEqual(arm["pending"], 8)
        self.assertEqual(rep["comparisons"][0]["verdict"]["label"], "inconclusive")

    def test_every_rate_has_an_interval_and_an_n(self):
        for name in self.data:
            for arm in self.report(name)["arms"]:
                t = arm["pass_at_1"]["text"]
                self.assertTrue(re.search(r"95% CI .* to .*, n = \d+ tasks?, \d+ attempts?", t) or "no result" in t, t)

    def test_the_demo_source_answers_like_the_psql_source(self):
        s = DemoSource(7, 0)
        rows, ok = s.bench_experiments()
        self.assertTrue(ok)
        self.assertEqual([r["id"] for r in rows], ["demo-just-started", "demo-clear-gap", "demo-small-lift"])
        self.assertIsNone(s.bench_data("nope"))
        d = s.bench_data("demo-small-lift")
        d["attempts"].clear()
        self.assertTrue(s.bench_data("demo-small-lift")["attempts"])  # a copy: callers cannot change the demo


class ServiceTests(unittest.TestCase):
    def service(self, src=None, **kw):
        return BoardService(src or DemoSource(7, 0), ttl=0.0, **kw)

    def test_the_list_and_one_experiment(self):
        svc = self.service()
        lst = svc.bench_list_payload()
        self.assertTrue(lst["available"])
        self.assertTrue(lst["meta"]["demo"])
        self.assertEqual(lst["meta"]["banner"], "DEMO DATA, not your board")
        self.assertEqual(len(lst["experiments"]), 3)
        rep = svc.bench_payload("demo-clear-gap")
        self.assertEqual(rep["experiment"]["name"], "demo-clear-gap")
        self.assertIn("meta", rep)
        self.assertIsNone(svc.bench_payload("nope"))

    def test_the_report_is_cached_for_a_few_seconds(self):
        clock = [0.0]
        src = FakeSource()
        src.bench = generate_bench(7)
        svc = BoardService(src, ttl=0.0, clock=lambda: clock[0])
        svc.bench_payload("demo-small-lift")
        svc.bench_payload("demo-small-lift")
        self.assertEqual(src.calls.count("bench_data"), 1)
        clock[0] += 10
        svc.bench_payload("demo-small-lift")
        self.assertEqual(src.calls.count("bench_data"), 2)

    def test_concurrent_requests_for_one_report_compute_it_once(self):
        import threading
        import time as _t
        src = FakeSource()
        src.bench = generate_bench(7)
        orig = src.bench_data

        def slow(name):
            _t.sleep(0.3)
            return orig(name)
        src.bench_data = slow
        svc = BoardService(src, ttl=0.0)
        out = []
        ts = [threading.Thread(target=lambda: out.append(svc.bench_payload("demo-small-lift"))) for _ in range(6)]
        [t.start() for t in ts]
        [t.join() for t in ts]
        self.assertEqual(len(out), 6)
        self.assertEqual(src.calls.count("bench_data"), 1)

    def test_a_board_without_bench_tables_says_how_to_make_them(self):
        svc = self.service(FakeSource())
        lst = svc.bench_list_payload()
        self.assertFalse(lst["available"])
        self.assertIn("board init", lst["note"])

    def test_a_failing_source_is_an_error_for_the_caller(self):
        src = FakeSource()
        src.bench = {}
        src.error = SourceError("the database query failed")
        with self.assertRaises(SourceError):
            self.service(src).bench_payload("x")


class ApiTests(unittest.TestCase):
    def setUp(self):
        self.run = test_server.Running(DemoSource(7, 0))
        self.addCleanup(self.run.stop)

    def test_the_list(self):
        status, body, resp = self.run.get_json("/api/experiments")
        self.assertEqual(status, 200)
        self.assertEqual({e["name"] for e in body["experiments"]}, {"demo-small-lift", "demo-clear-gap", "demo-just-started"})
        self.assertEqual(resp.getheader("Cache-Control"), "no-store")
        self.assertEqual(resp.getheader("Content-Security-Policy"), test_server.EXPECTED_CSP)

    def test_one_experiment_with_its_comparison_grid_and_errors(self):
        status, rep, _ = self.run.get_json("/api/experiment?name=demo-clear-gap")
        self.assertEqual(status, 200)
        self.assertEqual([a["arm"] for a in rep["arms"]], ["baseline", "candidate"])
        self.assertEqual(rep["comparisons"][0]["verdict"]["label"], "b_better")
        self.assertEqual(len(rep["tasks"]), 40)
        cell = rep["grid"]["t01"]["baseline"]
        self.assertEqual(set(cell), {"n", "passes", "fails", "timeouts", "errors", "pending", "flaky", "tampered"})
        self.assertTrue(rep["errors"])

    def test_the_small_experiment_is_inconclusive_on_the_wire(self):
        _, rep, _ = self.run.get_json("/api/experiment?name=demo-small-lift")
        c = rep["comparisons"][0]
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertIn("Fewer than the minimum of 30 paired tasks", c["verdict"]["text"])

    def test_no_rate_on_the_wire_is_a_bare_percentage(self):
        for name in ("demo-small-lift", "demo-clear-gap", "demo-just-started"):
            _, rep, _ = self.run.get_json("/api/experiment?name=" + name)
            for arm in rep["arms"]:
                for key in ("pass_at_1", "pass_at_k", "pass_pow_k"):
                    text = arm[key]["text"]
                    if "%" in text:
                        self.assertRegex(text, r"n = \d+ task|over \d+ task", (name, key, text))
                self.assertRegex(arm["pass_at_1"]["text"], r"CI|no result")

    def test_the_rule_parameter(self):
        status, rep, _ = self.run.get_json("/api/experiment?name=demo-small-lift&rule=any")
        self.assertEqual((status, rep["rule"]), (200, "any"))
        self.assertEqual(self.run.get_json("/api/experiment?name=demo-small-lift&rule=most")[0], 400)

    def test_errors(self):
        self.assertEqual(self.run.get_json("/api/experiment?name=nope")[0], 404)
        self.assertEqual(self.run.get_json("/api/experiment")[0], 400)
        self.assertEqual(self.run.get_json("/api/experiment?name=a%27b")[0], 400)
        self.assertEqual(self.run.get_json("/api/experiment?name=" + "x" * 49)[0], 400)
        self.assertEqual(self.run.get_json("/api/experiment?name=a&name=b")[0], 400)
        self.assertEqual(self.run.get_json("/api/experiment?name=demo-clear-gap&x=1")[0], 400)
        self.assertEqual(self.run.get_json("/api/experiments?x=1")[0], 400)

    def test_it_only_reads(self):
        for method in ("POST", "PUT", "DELETE"):
            status, _, _ = self.run.request(method, "/api/experiments")
            self.assertEqual(status, 403 if method == "POST" else 405)  # POST: writes are off unless --allow-writes

    def test_a_failing_database_is_a_502_with_no_internals(self):
        src = FakeSource()
        src.bench = {}
        src.error = SourceError("the database query failed (password=hunter2)")
        run = test_server.Running(src)
        self.addCleanup(run.stop)
        status, body, _ = run.get_json("/api/experiments")
        self.assertEqual(status, 502)
        self.assertNotIn("hunter2", json.dumps(body))


class PageScriptTests(unittest.TestCase):
    def region(self):
        js = strip_comments(read("app.js"))
        start = js.index("function hrefExp(")
        end = js.index("function parseRoute()")
        return js[start:end]

    def test_the_screen_is_linked_and_routed(self):
        self.assertIn('href="#/experiments"', read("index.html"))
        js = read("app.js")
        self.assertIn('"#/experiments"', js)
        self.assertIn("makeExperimentScreen", js)

    def test_the_experiment_code_has_no_dangerous_construct(self):
        self.assertEqual(findings(self.region(), FORBIDDEN_JS), [])

    def test_the_page_formats_no_percentage_of_its_own(self):
        region = self.region()
        self.assertNotIn("pct(", region)
        self.assertNotRegex(region, r"\*\s*100")
        self.assertNotIn("toFixed(1)", region)

    def test_the_page_says_inconclusive_is_a_result_and_always_shows_n_and_the_interval(self):
        region = self.region()
        self.assertIn("Inconclusive is a result", region)
        self.assertIn("n = ", region)
        self.assertIn("interval", region)
        self.assertIn("pass_at_1.text", region)
        self.assertIn("verdict.text", region)

    def test_errors_are_listed_apart_and_marked_in_the_grid(self):
        region = self.region()
        self.assertIn("Harness errors", region)
        self.assertIn("left out of every rate", region)
        for marker in ("flaky", "pending", "tampered"):
            self.assertIn(marker, region)

    def test_board_text_reaches_the_page_only_as_text(self):
        region = self.region()
        for field in ("x.summary", "e.suite", "e.created_by", "e.name"):
            self.assertIn(field, region)
        self.assertNotIn("innerHTML", read("app.js"))


class AvailabilityTests(unittest.TestCase):
    def test_without_benchlib_the_screen_says_so(self):
        saved = bench._report
        bench._report = None
        try:
            self.assertFalse(bench.available())
            payload = bench.list_payload(DemoSource(7, 0), {"demo": True})
            self.assertFalse(payload["available"])
            self.assertIn("benchlib", payload["note"])
            self.assertIsNone(bench.experiment_payload(DemoSource(7, 0), "demo-small-lift", {}))
        finally:
            bench._report = saved


if __name__ == "__main__":
    unittest.main()
