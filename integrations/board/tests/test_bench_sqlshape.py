"""The SQL of the bench commands of the board helper, checked without a database.

The helper runs `psql` through BOARD_PSQL; these tests give it a fake that logs the SQL and the -v
variables and answers from canned rules. They check what the SQL rules of the board promise: values reach
SQL only as bound variables, every statement touches one table, a bad value is refused before psql is
started, a duplicate is reported as one, and bench-record is refused by the fence.

The SQL itself runs against a real latticed in the dev-stack job (tests/bench_devstack.py).

Run: python3 -m unittest discover -s integrations/board/tests
"""

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest

import benchbase
from benchbase import BOARD, BOARD_DIR

HERE = os.path.dirname(os.path.abspath(__file__))
FAKE = os.path.join(HERE, "fakepsql.py")
SHA = "a" * 64
IDF = ["--experiment", "e1", "--arm", "a", "--task", "k"]  # the attempt a record is about
TABLES = ("bench_experiments", "bench_arms", "bench_tasks", "bench_attempts", "tasks", "events", "counters", "task_leases")


class HelperCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bench-sql-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        psql = os.path.join(self.tmp, "psql")
        with open(psql, "w") as f:
            f.write('#!/bin/sh\nexec python3 "%s" "$@"\n' % FAKE)
        os.chmod(psql, 0o755)
        self.log = os.path.join(self.tmp, "calls.jsonl")
        self.state = os.path.join(self.tmp, "state")
        os.mkdir(self.state, 0o700)

    def run_board(self, args, rules=(), author="alice"):
        for p in (self.log, self.log + ".counts"):
            if os.path.exists(p):
                os.remove(p)
        with open(os.path.join(self.tmp, "rules.json"), "w") as f:
            json.dump([dict(r) if isinstance(r, dict) else {"match": r[0], "out": r[1]} for r in rules]
                      + [{"match": "SELECT experiment_id, arm, task_key FROM bench_attempts", "out": "e1\ta\tk"}], f)
        env = {"PATH": os.environ["PATH"], "HOME": self.tmp, "TMPDIR": self.tmp, "BOARD_PSQL": os.path.join(self.tmp, "psql"),
               "BOARD_STATE_DIR": self.state, "FAKE_LOG": self.log, "FAKE_RULES": os.path.join(self.tmp, "rules.json"),
               "BOARD_CONNECT_TRIES": "1"}
        if author:
            env["BOARD_AUTHOR"] = author
        r = subprocess.run(["sh", BOARD, *args], env=env, capture_output=True, text=True, timeout=60)
        calls = []
        if os.path.exists(self.log):
            with open(self.log) as f:
                calls = [json.loads(x) for x in f if x.strip()]
        return r, calls


def statements(sql):
    out, cur = [], []
    for line in sql.splitlines():
        s = line.strip()
        if not s or s.startswith("\\") or s in ("BEGIN;", "COMMIT;"):
            continue
        cur.append(s)
        if s.endswith(";") or s.endswith("\\gset"):
            out.append(" ".join(cur))
            cur = []
    return out


def vars_of(call):
    out = {}
    argv = call["argv"]
    for i, a in enumerate(argv):
        if a == "-v" and "=" in argv[i + 1]:
            k, v = argv[i + 1].split("=", 1)
            out[k] = v
    return out


class WritesAreBound(HelperCase):
    EVIL = "x'; DROP TABLE bench_attempts; --"

    def test_register_arm_task_plan_use_variables_only(self):
        cases = [
            (["bench-experiment", "exp1", "--suite", "s1", "--suite-sha", SHA, "--seed", "7", "--repeats", "3",
              "--planned", "24", "--harness", "board-bench/1"], [("SELECT id FROM tasks", "")]),
            (["bench-arm", "exp1", "base", "--agent", "cmd", "--config-sha", SHA], [("FROM bench_experiments", "exp1")]),
            (["bench-task", "exp1", "calc", "--digest", SHA], [("FROM bench_experiments", "exp1")]),
            (["bench-plan", "exp1", "base", "calc", "t0abc", "--repeat", "1", "--order", "5", "--harness", "h"],
             [("FROM tasks WHERE id", "t0abc"), ("FROM bench_experiments", "exp1")]),
        ]
        for args, rules in cases:
            with self.subTest(args[0]):
                r, calls = self.run_board(args, rules)
                self.assertEqual(r.returncode, 0, r.stderr)
                writes = [c for c in calls if re.search(r"\bINSERT INTO bench_", c["sql"])]
                self.assertEqual(len(writes), 1)
                sql = writes[0]["sql"]
                self.assertRegex(sql, r"VALUES \(:'")  # every value is a psql variable
                self.assertNotRegex(sql, r"VALUES \([^:)]*'[a-z0-9]")  # no literal user text
                for v in vars_of(writes[0]).values():
                    if len(v) > 4:
                        self.assertNotIn(v, sql)

    def test_a_hostile_summary_stays_in_a_variable(self):
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "fail", "--summary", self.EVIL.replace("'", "")],
                                  [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "fail")])
        self.assertEqual(r.returncode, 0, r.stderr)
        tx = [c for c in calls if "UPDATE bench_attempts" in c["sql"]][0]
        self.assertIn(":'summary'", tx["sql"])
        self.assertNotIn("DROP", tx["sql"])
        self.assertEqual(vars_of(tx)["summary"], self.EVIL.replace("'", ""))

    def test_every_statement_names_one_table_and_no_join_or_subquery(self):
        scripts = [
            (["bench-experiment", "e", "--suite", "s", "--suite-sha", SHA, "--seed", "1", "--repeats", "1", "--planned", "1", "--harness", "h"], []),
            (["bench-arm", "e", "a", "--agent", "cmd", "--config-sha", SHA], [("FROM bench_experiments", "e")]),
            (["bench-task", "e", "t", "--digest", SHA], [("FROM bench_experiments", "e")]),
            (["bench-plan", "e", "a", "t", "t1", "--repeat", "0", "--order", "0", "--harness", "h"],
             [("FROM tasks WHERE id", "t1"), ("FROM bench_experiments", "e")]),
            (["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass", "--cost", "0.5", "--tokens", "9", "--duration-ms", "3",
              "--grader-exit", "0", "--grader-runs", "p", "--summary", "ok", "--flaky", "1", "--tampered", "2"],
             [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "pass")]),
            (["bench-rows", "e"], [("FROM bench_experiments", "e\ts\t" + SHA + "\t1\t1\t1\th\tx\t1")]),
            (["bench-list"], []),
        ]
        for args, rules in scripts:
            r, calls = self.run_board(args, rules)
            self.assertEqual(r.returncode, 0, "%s: %s" % (args[0], r.stderr))
            for c in calls:
                for st in statements(c["sql"]):
                    if re.match(r"(?i)(SELECT|UPDATE|INSERT)", st) and "AS bigint" not in st:
                        tables = re.findall(r"(?i)\b(?:FROM|UPDATE|INTO)\s+([a-z_]+)", st)
                        self.assertLessEqual(len(set(tables)), 1, st)  # a constant SELECT names none
                        for t in tables:
                            self.assertIn(t, TABLES, st)
                        self.assertNotRegex(st, r"(?i)\bJOIN\b|\(\s*SELECT")

    def test_bench_record_needs_no_counter_bump(self):
        # like heartbeat, it writes one row and does not serialise every board writer
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"],
                                  [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "pass")])
        tx = [c for c in calls if "UPDATE bench_attempts" in c["sql"]][0]
        self.assertNotIn("UPDATE counters", tx["sql"])


class RecordShape(unittest.TestCase):
    def test_record_fence_guard(self):
        """The write of bench-record sits inside `\\if :ok`, after a guard that names the state, the owner and the
        fence (the same check sqlshape.check_all makes for the board's other writes)."""
        import sqlshape
        tmp = tempfile.mkdtemp(prefix="bench-shape-")
        self.addCleanup(shutil.rmtree, tmp, True)
        args, rules, _, _ = sqlshape.PATHS["bench-record"]
        calls = sqlshape._run(BOARD, args, list(rules) + sqlshape.COMMON_READS, tmp)
        tx = [c for c in calls if "UPDATE bench_attempts SET outcome" in c["sql"] and "BEGIN" in c["sql"]]
        self.assertEqual(len(tx), 1)
        self.assertEqual(sqlshape.check_script("bench-record", tx[0]["sql"]), [])


class Validation(HelperCase):
    def refused(self, args, fragment=None):
        r, calls = self.run_board(args)
        self.assertEqual(r.returncode, 1, (args, r.stderr))
        self.assertEqual(calls, [], "psql must not be started for %s" % args)
        if fragment:
            self.assertIn(fragment, r.stderr)

    def test_names_and_hashes(self):
        base = ["--suite", "s", "--suite-sha", SHA, "--seed", "1", "--repeats", "1", "--planned", "1", "--harness", "h"]
        self.refused(["bench-experiment", "bad name", *base], "may use only")
        self.refused(["bench-experiment", "x" * 49, *base], "longer than 48")
        self.refused(["bench-experiment", "e", "--suite", "s", "--suite-sha", "xyz", "--seed", "1", "--repeats", "1",
                      "--planned", "1", "--harness", "h"], "64 lower-case hex")
        self.refused(["bench-experiment", "e", "--suite", "s", "--suite-sha", "A" * 64, "--seed", "1", "--repeats", "1",
                      "--planned", "1", "--harness", "h"], "64 lower-case hex")
        self.refused(["bench-experiment", "e", *base[:6], "--repeats", "0", "--planned", "1", "--harness", "h"], "from 1 to 100")
        self.refused(["bench-experiment", "e", *base, "--bogus", "1"], "unknown option")
        self.refused(["bench-experiment"], "takes an experiment name")

    def test_record_fields(self):
        ok = ["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"]
        self.refused(["bench-record", "t1", *IDF, "--outcome", "pass"], "needs --fence")
        self.refused(["bench-record", "t1", "--fence", "x", *IDF, "--outcome", "pass"])
        self.refused(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "great"], "must be pass, fail, timeout or error")
        self.refused(["bench-record", "t1", "--fence", "5"], "must be pass, fail, timeout or error")
        self.refused(ok + ["--flaky", "2"], "0 or 1")
        for cost in ("1e3", "-1", "1.2.3", ".5", "5.", "abc", "1" * 20, "NULL", "0; DROP"):
            self.refused(ok + ["--cost", cost])
        self.refused(ok + ["--tokens", "-1"])
        self.refused(ok + ["--duration-ms", "1.5"])
        self.refused(ok + ["--grader-exit", "256"])
        self.refused(ok + ["--summary", "two\nlines"], "one line")
        self.refused(ok + ["--summary", "back\\slash"], "backslash")
        self.refused(ok + ["--summary", "x" * 700], "longer")
        self.refused(ok + ["--grader-runs", "p" * 65], "longer than 64")
        self.refused(["bench-record", "bad id", "--fence", "5", *IDF, "--outcome", "pass"], "not a task id")

    def test_writing_commands_need_an_author(self):
        for args in (["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"],
                     ["bench-plan", "e", "a", "t", "t1", "--repeat", "0", "--order", "0", "--harness", "h"]):
            r, calls = self.run_board(args, author=None)
            self.assertEqual(r.returncode, 1)
            self.assertIn("BOARD_AUTHOR is required", r.stderr)
            self.assertEqual(calls, [])

    def test_reads_need_none(self):
        r, _ = self.run_board(["bench-list"], author=None)
        self.assertEqual(r.returncode, 0, r.stderr)


class Behaviour(HelperCase):
    def test_a_duplicate_name_is_said_to_be_taken(self):
        rules = [{"match": "INSERT INTO bench_experiments", "out": "", "rc": 1,
                  "err": "ERROR:  23505: duplicate key value violates unique constraint"}]
        r, _ = self.run_board(["bench-experiment", "e", "--suite", "s", "--suite-sha", SHA, "--seed", "1", "--repeats", "1",
                               "--planned", "1", "--harness", "h"], rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("already exists", r.stderr)

    def test_an_unknown_experiment_is_refused_before_the_insert(self):
        r, calls = self.run_board(["bench-arm", "ghost", "a", "--agent", "cmd", "--config-sha", SHA])
        self.assertEqual(r.returncode, 1)
        self.assertIn("no such experiment", r.stderr)
        self.assertFalse([c for c in calls if "INSERT" in c["sql"]])

    def test_plan_needs_the_board_task(self):
        r, calls = self.run_board(["bench-plan", "e", "a", "t", "t1", "--repeat", "0", "--order", "0", "--harness", "h"],
                                  [("FROM bench_experiments", "e")])
        self.assertEqual(r.returncode, 1)
        self.assertIn("no such task", r.stderr)

    def test_a_refused_fence_exits_3_and_leaves_a_fenced_event(self):
        rules = [("UPDATE bench_attempts", "f"), ("SELECT state, coalesce\\(owner", "claimed\tbob\t9")]
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        self.assertEqual(r.returncode, 3)
        self.assertIn("lease lost", r.stderr)
        self.assertTrue([c for c in calls if "'fenced'" in c["sql"]])
        self.assertFalse([c for c in calls if "SELECT outcome FROM bench_attempts" in c["sql"]])

    def test_recording_twice_says_so_and_exits_0(self):
        rules = [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "fail")]
        r, _ = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        self.assertEqual(r.returncode, 0)
        self.assertIn("already recorded t1: fail (nothing changed)", r.stdout)
        r, _ = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "fail"], rules)
        self.assertIn("recorded t1: fail", r.stdout)

    def test_a_result_for_another_attempt_is_refused_before_any_write(self):
        rules = [("SELECT experiment_id, arm, task_key FROM bench_attempts", "e1\tother\tk")]
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("planned for e1/other/k", r.stderr)
        self.assertIn("nothing written", r.stderr)
        self.assertFalse([c for c in calls if "UPDATE" in c["sql"]])

    def test_the_update_names_the_identity_as_well_as_the_task(self):
        rules = [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "pass")]
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        tx = [c for c in calls if "UPDATE bench_attempts" in c["sql"]][0]
        self.assertIn("experiment_id = :'exp' AND arm = :'arm' AND task_key = :'key'", tx["sql"])
        self.assertEqual((vars_of(tx)["exp"], vars_of(tx)["arm"], vars_of(tx)["key"]), ("e1", "a", "k"))

    def test_record_needs_the_identity(self):
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", "--outcome", "pass"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("may use only", r.stderr)
        self.assertEqual(calls, [])

    def test_a_task_that_is_not_a_bench_attempt_is_refused(self):
        rules = [("SELECT experiment_id, arm, task_key FROM bench_attempts", ""), ("UPDATE bench_attempts", "t")]
        r, _ = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("not a bench attempt", r.stderr)

    def test_null_fields_are_sent_as_null_and_numbers_as_numbers(self):
        rules = [("UPDATE bench_attempts", "t"), ("SELECT outcome FROM bench_attempts", "pass")]
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass"], rules)
        v = vars_of([c for c in calls if "UPDATE bench_attempts" in c["sql"]][0])
        self.assertEqual((v["cost"], v["tokens"], v["duration"], v["gexit"]), ("NULL", "NULL", "NULL", "NULL"))
        r, calls = self.run_board(["bench-record", "t1", "--fence", "5", *IDF, "--outcome", "pass", "--cost", "0.0421", "--tokens", "9",
                                   "--duration-ms", "12", "--grader-exit", "0"], rules)
        v = vars_of([c for c in calls if "UPDATE bench_attempts" in c["sql"]][0])
        self.assertEqual((v["cost"], v["tokens"], v["duration"], v["gexit"]), ("0.0421", "9", "12", "0"))

    def test_rows_output_is_tagged_and_in_the_documented_shape(self):
        T = "\t"
        rules = [("FROM bench_experiments", T.join(["e1", "s1", SHA, "7", "2", "4", "h", "bob", "100"])),
                 ("FROM bench_arms", "base" + T + "cmd" + T + SHA),
                 ("FROM bench_tasks", "calc" + T + SHA),
                 ("FROM bench_attempts", T.join(["t1", "base", "calc", "0", "1", "pass", "0", "0", "15", "0.5", "", "0", "p", "h", "99", "ok"]))]
        r, _ = self.run_board(["bench-rows", "e1"], rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = r.stdout.splitlines()
        self.assertEqual([x.split("\t")[0] for x in lines], ["experiment", "arm", "task", "attempt"])
        from benchlib import report
        d = report.parse_rows(r.stdout)
        self.assertEqual(d["attempts"][0]["cost_usd"], 0.5)
        self.assertIsNone(d["attempts"][0]["tokens"])

    def test_rows_of_an_unknown_experiment(self):
        r, _ = self.run_board(["bench-rows", "ghost"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("no such experiment", r.stderr)

    def test_the_schema_has_the_four_tables_and_the_board_applies_it(self):
        with open(os.path.join(BOARD_DIR, "schema.sql")) as f:
            schema = f.read()
        for t in ("bench_experiments", "bench_arms", "bench_tasks", "bench_attempts"):
            self.assertIn("CREATE TABLE IF NOT EXISTS %s (" % t, schema)
        code = "\n".join(ln for ln in schema.splitlines() if not ln.lstrip().startswith("--"))
        self.assertNotRegex(code, r"(?i)alter table|references|foreign key|\bjoin\b")
        # no fence in the data
        attempts = schema.split("CREATE TABLE IF NOT EXISTS bench_attempts (")[1].split(");")[0]
        self.assertNotIn("fence", attempts)
        for col in ("duration_ms bigint,", "cost_usd double precision,", "tokens bigint,"):
            self.assertIn(col, attempts)  # no NOT NULL: unknown stays NULL


if __name__ == "__main__":
    unittest.main()
