"""Planted faults: each one is put into a copy of board bench and the tests must notice.

A test suite that has never failed proves little. For each fault below this file copies integrations/board
(the helper, the runner, board-bench and benchlib), changes one thing the way a real bug would, and runs
the tests that are meant to catch it against the copy (BENCH_UNDER_TEST). The copy must fail. A control
run on an unchanged copy must pass, so a failure means the fault was seen, not that the copy is broken.

Faults: protected paths not restored; a harness error counted as an agent failure (in the report and in the
grader); an attempt recorded without the lease check; pass@k off by one; a rate printed with no interval;
a fixture digest not verified; the suite not verified; arms and cells not randomised; a flaky grader hidden;
the verdict ignoring the minimum n; an unseeded bootstrap; the task-level interval replaced by the naive one;
a one-sided exact test; pending attempts counted; the agent given the whole environment; a lost lease ignored
by the runner's hook; a suite read from outside the allowed directory; the exact test ignored in the verdict;
the Wilson, McNemar, Bonferroni, bootstrap and `solved` formulas; a protected hard link; an edited pristine copy; 126/127 as errors;
the grader sharing the agent's HOME; a result recorded for another attempt.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REAL = os.path.dirname(HERE)
COPY = ("board", "board-run", "board-bench", "board-mcp", "schema.sql", "benchlib", "bench")

# (label, file in the board directory, text to replace, replacement, [(test module, -k pattern or None)])
FAULTS = [
    ("protected paths are not restored before grading", "benchlib/attempt.py",
     'changed = restore_protected(work, pristine, task["grader"]["protected"])', "changed = []",
     [("test_bench_attempt", "cheat|deleted|link|added|executable")]),
    ("a harness error in the report is counted as a failure", "benchlib/report.py",
     'n = c["pass"] + c["fail"] + c["timeout"]', 'n = c["pass"] + c["fail"] + c["timeout"] + c["error"]',
     [("test_bench_stats", "error")]),
    ("a grader that could not run is a failure, not an error", "benchlib/attempt.py",
     "elif rc in GRADER_ERROR_EXITS:", "elif False:",
     [("test_bench_attempt", "125|126|exits")]),
    ("an attempt is recorded without checking the lease", "board",
     "SELECT (state = 'claimed' AND coalesce(owner, '') = :'author' AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts",
     "SELECT (true) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts",
     [("test_bench_sqlshape", "fence_guard")]),
    ("an attempt is recorded outside the guard block", "board",
     "\\if :ok\nUPDATE bench_attempts", "UPDATE bench_attempts",
     [("test_bench_sqlshape", "fence_guard")]),
    ("pass@k is off by one", "benchlib/stats.py",
     "return 1.0 - math.comb(n - c, k) / math.comb(n, k)", "return 1.0 - math.comb(n - c, k + 1) / math.comb(n, k)",
     [("test_bench_stats", "PassAtK")]),
    ("pass^k uses n - 1", "benchlib/stats.py",
     "return math.comb(c, k) / math.comb(n, k)", "return math.comb(c, k) / math.comb(n - 1, k)",
     [("test_bench_stats", "PassPowK")]),
    ("a rate is printed without its interval and n", "benchlib/report.py",
     'return "%s (%s), n = %d task%s, %d attempt%s" % (pct(value), span, n_tasks, "" if n_tasks == 1 else "s",\n                                                     n_attempts, "" if n_attempts == 1 else "s")',
     "return pct(value)",
     [("test_bench_stats", "NoBarePercentage")]),
    ("the fixture digest is not verified", "benchlib/attempt.py",
     'if got != task["fixture"]["digest"]:', "if False:",
     [("test_bench_attempt", "digest")]),
    ("the suite sha is not verified", "benchlib/attempt.py",
     'if sha != body["sha"]:', "if False:",
     [("test_bench_attempt", "edited_after")]),
    ("the cells are not randomised", "benchlib/suite.py",
     "for task, rep in shuffled(cells, rng):", "for task, rep in cells:",
     [("test_bench_suite", "MatrixTests")]),
    ("the arms inside a cell are not randomised", "benchlib/suite.py",
     'for arm in shuffled([a["id"] for a in suite["arms"]], rng):', 'for arm in [a["id"] for a in suite["arms"]]:',
     [("test_bench_suite", "MatrixTests")]),
    ("a flaky grader is hidden", "benchlib/attempt.py",
     '"flaky": 0 < passes < len(kinds)', '"flaky": False',
     [("test_bench_attempt", "flaky|disagree|split")]),
    ("flaky attempts are not counted in the report", "benchlib/report.py",
     '"flaky": sum(1 for r in rows if r["flaky"] and r["outcome"] in VALID)', '"flaky": 0',
     [("test_bench_cli", "EndToEnd")]),
    ("the verdict ignores the minimum number of tasks", "benchlib/report.py",
     "    if n < mn:", "    if False:",
     [("test_bench_stats", "Verdict")]),
    ("the bootstrap is not seeded", "benchlib/stats.py",
     "rng = random.Random(seed)", "rng = random.Random()",
     [("test_bench_stats", "same_seed")]),
    ("the interval counts attempts, not tasks", "benchlib/report.py",
     "ci = stats.task_level_wilson(rates) if rates else None", "ci = stats.wilson(passes, n_att) if n_att else None",
     [("test_bench_stats", "ReportByHand|tasks")]),
    ("the exact test is one-sided", "benchlib/stats.py",
     "return min(1.0, 2.0 * tail)", "return min(1.0, tail)",
     [("test_bench_stats", "McNemar")]),
    ("attempts not recorded yet are counted as failures", "benchlib/report.py",
     'n = c["pass"] + c["fail"] + c["timeout"]', 'n = c["pass"] + c["fail"] + c["timeout"] + c["planned"]',
     [("test_bench_stats", "pending")]),
    ("the agent gets the whole environment", "benchlib/attempt.py",
     "env = {n: os.environ[n] for n in base_names if n in os.environ}", "env = dict(os.environ)",
     [("test_bench_attempt", "environment")]),
    ("the runner ignores a lost lease at the after-cmd", "board-run",
     "if hook_rc == 3:", "if False:",
     [("test_run_after", "exits_3")]),
    ("the runner marks the task done after a failed after-cmd", "board-run",
     "elif hook_rc != 0:", "elif False:",
     [("test_run_after", "fails_the_task")]),
    ("a suite outside the allowed directory is run", "benchlib/attempt.py",
     "if not real.startswith(root + os.sep):", "if False:",
     [("test_bench_attempt", "outside_the_allowed")]),
    ("the agent timeout does not kill the process group", "benchlib/attempt.py",
     ("R.kill_group(proc, grace)", "R.signal_group(proc.pid, signal.SIGKILL)  # anything the child left running in its group"),
     ("proc.kill()", "pass"),
     [("test_bench_attempt", "process_group")]),
    ("the Wilson centre loses its z^2 / 2n term", "benchlib/stats.py",
     "centre = (p + z2 / (2.0 * n)) / denom", "centre = p / denom",
     [("test_bench_stats", "WilsonTests")]),
    ("the exact test counts one tail value too few", "benchlib/stats.py",
     "for i in range(hi, m + 1)", "for i in range(hi + 1, m + 1)",
     [("test_bench_stats", "McNemar")]),
    ("Bonferroni is not applied", "benchlib/stats.py",
     "return min(1.0, p * max(comparisons, 1))", "return min(1.0, p)",
     [("test_bench_stats", "bonferroni")]),
    ("the bootstrap pairs the wrong tasks", "benchlib/stats.py",
     "[b - a for a, b in zip(rates_a, rates_b)]", "[b - a for a, b in zip(rates_a, rates_b[::-1])]",
     [("test_bench_stats", "pairing|paired_difference")]),
    ("a tie counts as solved under the majority rule", "benchlib/report.py",
     'return counts["passes"] * 2 > counts["n"]', 'return counts["passes"] * 2 >= counts["n"]',
     [("test_bench_stats", "ComparisonByHand")]),
    ("the bootstrap percentile index is off by one", "benchlib/stats.py",
     "idx = min(len(sorted_vals) - 1, max(0, int(q * len(sorted_vals))))",
     "idx = min(len(sorted_vals) - 1, max(0, int(q * len(sorted_vals)) + 1))",
     [("test_bench_stats", "percentile_rule")]),
    ("the verdict reads the wrong end of the interval", "benchlib/report.py",
     'if d["lo"] > 0:\n        label, said', 'if d["hi"] > 0:\n        label, said',
     [("test_bench_stats", "Verdict")]),
    ("the verdict ignores the exact test", "benchlib/report.py",
     "if p_adj is None or p_adj >= ALPHA:", "if False:",
     [("test_bench_stats", "SignTestGate")]),
    ("a protected path is not removed before it is replaced", "benchlib/attempt.py",
     "        if os.path.lexists(dst):\n            _remove(dst)\n        shutil.copyfile(", "        shutil.copyfile(",
     [("test_bench_attempt", "link")]),
    ("the pristine copy is not verified again before restoring", "benchlib/attempt.py",
     'if suitemod.tree_digest(pristine) != task["fixture"]["digest"]:', "if False:",
     [("test_bench_attempt", "pristine")]),
    ("exits 126 and 127 are harness errors", "benchlib/attempt.py",
     "GRADER_ERROR_EXITS = (125,)", "GRADER_ERROR_EXITS = (125, 126, 127)",
     [("test_bench_attempt", "126")]),
    ("the grader shares the agent's HOME", "benchlib/attempt.py",
     "g = grade(task, work, suite_dir, ghome, tdir, redactor)", "g = grade(task, work, suite_dir, home, tdir, redactor)",
     [("test_bench_attempt", "home_of_its_own")]),
    ("a result for another attempt is recorded", "board",
     '[ "$OUT" = "$BO_experiment$TAB$BO_arm$TAB$BO_task" ] \\\n', 'true \\\n',
     [("test_bench_sqlshape", "another_attempt")]),
    ("a protected path that cannot be restored is a harness error", "benchlib/attempt.py",
     "except (TamperError, OSError, suitemod.SuiteError) as e:", "except (TamperError, suitemod.SuiteError) as e:",
     [("test_bench_attempt", "unwritable")]),
    ("lower bounds are rounded up", "benchlib/report.py",
     'return ("%.1f%%" % _tenths(p, False)) if (p < 10 or p > 90) else ("%d%%" % _whole(p, False))',
     'return ("%.1f%%" % _tenths(p, True)) if (p < 10 or p > 90) else ("%d%%" % _whole(p, True))',
     [("test_bench_stats", "RoundingTests")]),
    ("a changed pinned grader file is not noticed", "benchlib/attempt.py",
     "        if bad:\n            kinds.append", "        if False:\n            kinds.append",
     [("test_bench_attempt", "pinned_grader_file")]),
    ("protected globs are case-sensitive", "benchlib/attempt.py",
     '"(?:/.*)?$", re.IGNORECASE)', '"(?:/.*)?$")',
     [("test_bench_attempt", "ignores_case")]),
    ("the suite directory is taken from the unresolved path", "benchlib/attempt.py",
     "suite_dir = os.path.dirname(real)", 'suite_dir = os.path.dirname(body["suite"])',
     [("test_bench_attempt", "suite_dir_and_work")]),
    ("the verdict hides the dropped tasks", "benchlib/report.py",
     "    if cmp.get(\"dropped_tasks\"):", "    if False:",
     [("test_bench_stats", "dropped_from_the_pairing")]),
    ("the harness errors are not listed", "benchlib/report.py",
     'for a in data["attempts"] if a["outcome"] == "error"]', 'for a in data["attempts"] if False]',
     [("test_bench_stats", "error")]),
    ("a taken experiment name is not detected", "board",
     "*23505:*) die \"already exists (the name is taken)\" ;;", "*23505:*) return 0 ;;",
     [("test_bench_sqlshape", "duplicate")]),
]


def run_tests(copy_dir, module, pattern):
    env = {"PATH": os.environ["PATH"], "HOME": copy_dir, "BENCH_UNDER_TEST": copy_dir,
           "BOARD_RUN_UNDER_TEST": os.path.join(copy_dir, "board-run")}
    cmd = [sys.executable, "-m", "unittest", "-q"]
    for part in (pattern.split("|") if pattern else []):
        cmd += ["-k", part]
    return subprocess.run(cmd + [module], cwd=HERE, env=env, capture_output=True, text=True, timeout=600)


class PlantedFaults(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="bench-mut-")
        cls.control = os.path.join(cls.tmp, "control")
        cls.copy_into(cls.control)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, True)

    @staticmethod
    def copy_into(dest):
        os.makedirs(dest)
        for name in COPY:
            src = os.path.join(REAL, name)
            if os.path.isdir(src):
                shutil.copytree(src, os.path.join(dest, name), ignore=shutil.ignore_patterns("__pycache__"))
            elif os.path.exists(src):
                shutil.copy2(src, os.path.join(dest, name))

    def test_the_control_copy_passes_the_tests_the_faults_are_run_against(self):
        for module in sorted({m for *_, tests in FAULTS for m, _ in tests}):
            r = run_tests(self.control, module, None)
            self.assertEqual(r.returncode, 0, "%s failed on an unchanged copy:\n%s" % (module, r.stderr[-1500:]))

    def test_every_fault_is_caught(self):
        for i, (label, rel, old, new, tests) in enumerate(FAULTS):
            with self.subTest(label):
                dest = os.path.join(self.tmp, "m%d" % i)
                self.copy_into(dest)
                path = os.path.join(dest, rel)
                with open(path) as f:
                    src = f.read()
                for o, n in (zip(old, new) if isinstance(old, tuple) else [(old, new)]):
                    self.assertIn(o, src, "the fault's target is gone from %s: %r" % (rel, o[:70]))
                    src = src.replace(o, n, 1)
                with open(path, "w") as f:
                    f.write(src)
                caught = []
                for module, pattern in tests:
                    r = run_tests(dest, module, pattern)
                    caught.append(r.returncode != 0)
                self.assertTrue(all(caught), "not caught by %s: %s" % (tests, label))
                shutil.rmtree(dest, True)

    def test_the_list_covers_the_faults_the_brief_names(self):
        text = " ".join(f[0] for f in FAULTS)
        for needle in ("protected paths", "harness error", "without checking the lease", "off by one", "without its interval",
                       "fixture digest", "not randomised", "flaky grader is hidden"):
            self.assertIn(needle, text)


if __name__ == "__main__":
    unittest.main()
