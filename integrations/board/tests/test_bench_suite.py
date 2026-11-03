"""The suite file: a strict parser, the fixture digest, the attempt matrix and the plan."""

import copy
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

import benchbase
from benchbase import BENCH, EXAMPLE_SUITE
from benchlib import attempt, suite


def base_suite():
    with open(EXAMPLE_SUITE, encoding="utf-8") as f:
        return json.load(f)


def parse(doc):
    return suite.parse_suite(json.dumps(doc).encode())


class ParserTests(unittest.TestCase):
    def refuses(self, doc, fragment):
        with self.assertRaises(suite.SuiteError) as cm:
            parse(doc)
        self.assertIn(fragment, str(cm.exception))

    def test_the_example_suite_is_valid(self):
        s, sha, path = suite.load_suite(EXAMPLE_SUITE)
        self.assertEqual((len(s["tasks"]), len(s["arms"]), s["repeats"]), (4, 2, 2))
        self.assertEqual(len(sha), 64)
        self.assertEqual(s["tasks"][0]["grader"]["protected"], ["tests/**"])
        self.assertEqual(s["tasks"][2]["grader"]["reruns"], 2)
        self.assertEqual(s["tasks"][0]["grader"]["reruns"], 0)  # defaults are filled in
        self.assertEqual(s["tasks"][0]["fixture"]["digest"], "71ef44373260081d06e98624c88ea75613625d798c7ae7861faa27251d71905a")

    def test_unknown_keys_are_refused_at_every_level(self):
        d = base_suite()
        d["extra"] = 1
        self.refuses(d, "unknown key extra")
        d = base_suite()
        d["tasks"][0]["prompts"] = "plural"
        self.refuses(d, "unknown key prompts")
        d = base_suite()
        d["tasks"][0]["grader"]["timeout"] = 5
        self.refuses(d, "unknown key timeout")
        d = base_suite()
        d["arms"][0]["args"]["shell"] = "rm -rf"
        self.refuses(d, "unknown key shell")
        d = base_suite()
        d["tasks"][0]["fixture"]["tree"] = "x"
        self.refuses(d, "unknown key tree")

    def test_duplicate_keys_nan_and_garbage_are_refused(self):
        for raw, frag in ((b'{"suite": "a", "suite": "b"}', "duplicate key"), (b'{"suite": NaN}', "not JSON"),
                          (b"[1,2]", "must be an object"), (b"{", "not valid JSON"), (b"\xff\xfe", "not valid JSON")):
            with self.assertRaises(suite.SuiteError) as cm:
                suite.parse_suite(raw)
            self.assertIn(frag, str(cm.exception), raw)

    def test_oversized_file_and_oversized_matrix_are_refused(self):
        with self.assertRaises(suite.SuiteError):
            suite.parse_suite(b" " * (suite.MAX_SUITE_BYTES + 1))
        d = base_suite()
        d["tasks"] = [dict(copy.deepcopy(d["tasks"][0]), id="t%d" % i) for i in range(200)]
        d["arms"] = [dict(copy.deepcopy(d["arms"][0]), id="a%d" % i) for i in range(6)]
        d["repeats"] = 3
        self.refuses(d, "the matrix has 3600 attempts")
        d = base_suite()
        d["repeats"] = 21
        self.refuses(d, "repeats must be a whole number from 1 to 20")
        d = base_suite()
        d["arms"] = [dict(copy.deepcopy(d["arms"][0]), id="a%d" % i) for i in range(9)]
        self.refuses(d, "arms must be a list of 1 to 8")

    def test_ids_are_names_not_paths(self):
        d = base_suite()
        d["tasks"][0]["id"] = "../x"
        self.refuses(d, "character that is not allowed")
        d = base_suite()
        d["arms"][1]["id"] = d["arms"][0]["id"]
        self.refuses(d, "two arms have the same id")
        d = base_suite()
        d["tasks"][1]["id"] = d["tasks"][0]["id"]
        self.refuses(d, "two tasks have the same id")

    def test_fixture_rules(self):
        d = base_suite()
        d["tasks"][0]["fixture"] = {"git": {"url": "https://x", "ref": "main"}, "dir": "x", "digest": "0" * 64}
        self.refuses(d, "git fixture is not supported")
        for bad in ("/etc", "../x", "a/../../b", "a\\b"):
            d = base_suite()
            d["tasks"][0]["fixture"]["dir"] = bad
            self.refuses(d, "relative path inside the suite directory")
        d = base_suite()
        d["tasks"][0]["fixture"]["digest"] = "abc"
        self.refuses(d, "sha256:<64 hex digits>")
        d = base_suite()
        del d["tasks"][0]["fixture"]["digest"]
        self.refuses(d, "digest is required")

    def test_protected_globs(self):
        for bad in ("../tests/**", "/tests", "tests//x", "a\\b", "tests/", "x y"):
            d = base_suite()
            d["tasks"][0]["grader"]["protected"] = [bad]
            self.refuses(d, "protected glob")
        d = base_suite()
        d["tasks"][0]["grader"]["protected"] = ["a"] * 21
        self.refuses(d, "at most 20 globs")

    def test_grader_bounds(self):
        d = base_suite()
        d["tasks"][0]["grader"]["cmd"] = []
        self.refuses(d, "list of 1 to 32 strings")
        d = base_suite()
        d["tasks"][0]["grader"]["reruns"] = 5
        self.refuses(d, "reruns must be a whole number from 0 to 4")
        d = base_suite()
        d["tasks"][0]["grader"]["timeout_s"] = 0
        self.refuses(d, "timeout_s must be a whole number from 1 to 3600")
        d = base_suite()
        d["tasks"][0]["timeout_s"] = True
        self.refuses(d, "timeout_s must be a whole number")

    def test_grader_scripts_in_the_suite_directory_must_be_pinned(self):
        d = base_suite()
        d["tasks"][2]["grader"]["files"] = {}
        self.refuses(d, "must be pinned in grader.files")
        d = base_suite()
        d["tasks"][2]["grader"]["files"] = {"graders/flaky.py": "sha256:abc"}
        self.refuses(d, "must be sha256:<64 hex digits>")
        for bad in ("../x.py", "/etc/passwd", "a/*.py", "a//b"):
            d = base_suite()
            d["tasks"][2]["grader"]["files"]["graders/flaky.py"] = d["tasks"][2]["grader"]["files"].pop("graders/flaky.py")
            d["tasks"][2]["grader"]["files"][bad] = "sha256:" + "0" * 64
            self.refuses(d, "plain relative path")
        d = base_suite()
        self.assertEqual(list(parse(d)["tasks"][2]["grader"]["files"]), ["graders/flaky.py"])

    def test_plan_checks_the_pins(self):
        self.assertEqual(suite.verify_pins(os.path.dirname(EXAMPLE_SUITE),
                                           parse(base_suite())["tasks"][2]["grader"]["files"]), [])
        problems = suite.verify_pins(os.path.dirname(EXAMPLE_SUITE), {"graders/flaky.py": "0" * 64, "nope.py": "0" * 64})
        self.assertEqual(len(problems), 2)
        self.assertIn("is missing", problems[1])

    def test_arm_arguments_are_allow_listed(self):
        d = base_suite()
        d["arms"][0] = {"id": "c", "agent": "claude", "args": {"permission_mode": "bypassPermissions"}}
        self.refuses(d, "permission_mode must be one of")
        d["arms"][0] = {"id": "c", "agent": "claude", "args": {"allowed_tools": "Read,Bash"}}
        self.refuses(d, "shell without a command pattern")
        d["arms"][0] = {"id": "c", "agent": "claude", "args": {"allowed_tools": "Read,Bash(git diff *)", "model": "m-1", "budget_usd": 1.5}}
        self.assertEqual(parse(d)["arms"][0]["args"]["budget_usd"], 1.5)
        d["arms"][0] = {"id": "c", "agent": "claude", "args": {"budget_usd": 1000}}
        self.refuses(d, "budget_usd")
        d["arms"][0] = {"id": "c", "agent": "codex", "args": {"sandbox": "danger-full-access"}}
        self.refuses(d, "sandbox must be one of")
        d["arms"][0] = {"id": "c", "agent": "codex", "args": {"permission_mode": "dontAsk"}}
        self.refuses(d, "unknown key permission_mode")
        d["arms"][0] = {"id": "c", "agent": "gemini", "args": {}}
        self.refuses(d, "agent must be claude, codex or cmd")
        d["arms"][0] = {"id": "c", "agent": "cmd", "args": {}}
        self.refuses(d, "argv")
        d["arms"][0] = {"id": "c", "agent": "claude", "args": {"model": "x; rm -rf /"}}
        self.refuses(d, "character that is not allowed")

    def test_env_allow_never_passes_credentials_or_loader_variables(self):
        for name in ("PGPASSWORD", "BOARD_AUTHOR", "LATTICE_BOARD_URL", "TASK_FENCE", "BENCH_PROMPT_FILE", "LD_PRELOAD",
                     "DYLD_INSERT_LIBRARIES", "PYTHONPATH", "NODE_OPTIONS", "BASH_ENV"):
            d = base_suite()
            d["arms"][0]["env_allow"] = [name]
            self.refuses(d, "is reserved")
        d = base_suite()
        d["arms"][0]["env_allow"] = ["ANTHROPIC_API_KEY", "HOME"]
        self.assertEqual(parse(d)["arms"][0]["env_allow"], ["ANTHROPIC_API_KEY", "HOME"])
        d["arms"][0]["env_allow"] = ["bad name"]
        self.refuses(d, "bad variable name")


class DigestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bench-digest-")
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def put(self, rel, data=b"x", mode=0o644):
        p = os.path.join(self.tmp, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(data)
        os.chmod(p, mode)

    def test_digest_by_hand(self):
        # one line per file: path, NUL, 0 or 1 for the executable bit, NUL, sha256 of the content, newline
        self.put("a.txt", b"hi\n")
        line = "a.txt\x000\x00" + hashlib.sha256(b"hi\n").hexdigest() + "\n"
        self.assertEqual(suite.tree_digest(self.tmp), hashlib.sha256(line.encode()).hexdigest())

    def test_two_files_in_path_order(self):
        self.put("b/z.txt", b"z")
        self.put("a.txt", b"a", 0o755)
        lines = ("a.txt\x001\x00" + hashlib.sha256(b"a").hexdigest() + "\n"
                 + "b/z.txt\x000\x00" + hashlib.sha256(b"z").hexdigest() + "\n")
        self.assertEqual(suite.tree_digest(self.tmp), hashlib.sha256(lines.encode()).hexdigest())

    def test_content_name_and_executable_bit_each_change_it(self):
        self.put("a.txt", b"one")
        d0 = suite.tree_digest(self.tmp)
        self.put("a.txt", b"two")
        d1 = suite.tree_digest(self.tmp)
        os.chmod(os.path.join(self.tmp, "a.txt"), 0o755)
        d2 = suite.tree_digest(self.tmp)
        os.rename(os.path.join(self.tmp, "a.txt"), os.path.join(self.tmp, "b.txt"))
        d3 = suite.tree_digest(self.tmp)
        self.assertEqual(len({d0, d1, d2, d3}), 4)

    def test_timestamps_do_not_matter(self):
        self.put("a.txt", b"one")
        d0 = suite.tree_digest(self.tmp)
        os.utime(os.path.join(self.tmp, "a.txt"), (1, 1))
        self.assertEqual(suite.tree_digest(self.tmp), d0)

    def test_symbolic_links_are_refused(self):
        self.put("a.txt", b"one")
        os.symlink("/etc/passwd", os.path.join(self.tmp, "link"))
        with self.assertRaises(suite.SuiteError):
            suite.tree_digest(self.tmp)
        os.unlink(os.path.join(self.tmp, "link"))
        os.symlink("/tmp", os.path.join(self.tmp, "dirlink"))
        with self.assertRaises(suite.SuiteError):
            suite.tree_digest(self.tmp)

    def test_fixture_dir_must_stay_inside_the_suite_directory(self):
        outside = tempfile.mkdtemp(prefix="bench-outside-")
        self.addCleanup(shutil.rmtree, outside, True)
        os.symlink(outside, os.path.join(self.tmp, "escape"))
        with self.assertRaises(suite.SuiteError):
            suite.fixture_root(self.tmp, "escape")
        with self.assertRaises(suite.SuiteError):
            suite.fixture_root(self.tmp, "missing")

    def test_size_bounds(self):
        for i in range(suite.MAX_FIXTURE_FILES + 1):
            if i % 1000 == 0:
                os.makedirs(os.path.join(self.tmp, "d%d" % (i // 1000)), exist_ok=True)
            with open(os.path.join(self.tmp, "d%d" % (i // 1000), "f%d" % i), "w") as f:
                f.write("")
        with self.assertRaises(suite.SuiteError):
            suite.list_tree(self.tmp)

    def test_the_example_fixtures_match_the_digests_in_the_example_suite(self):
        s, _, path = suite.load_suite(EXAMPLE_SUITE)
        for t in s["tasks"]:
            root = suite.fixture_root(os.path.dirname(path), t["fixture"]["dir"])
            self.assertEqual(suite.tree_digest(root), t["fixture"]["digest"], t["id"])


class MatrixTests(unittest.TestCase):
    def setUp(self):
        d = base_suite()
        d["repeats"] = 4
        self.s = parse(d)

    def test_every_cell_once(self):
        m = suite.build_matrix(self.s, 11)
        self.assertEqual(len(m), 4 * 2 * 4)
        self.assertEqual(len({(x["task"], x["arm"], x["repeat"]) for x in m}), len(m))
        self.assertEqual([x["order"] for x in m], list(range(len(m))))

    def test_same_seed_same_order_other_seed_other_order(self):
        self.assertEqual(suite.build_matrix(self.s, 5), suite.build_matrix(self.s, 5))
        self.assertNotEqual(suite.build_matrix(self.s, 5), suite.build_matrix(self.s, 6))

    def test_arms_of_a_cell_are_adjacent_so_arms_interleave(self):
        m = suite.build_matrix(self.s, 3)
        for i in range(0, len(m), 2):
            self.assertEqual((m[i]["task"], m[i]["repeat"]), (m[i + 1]["task"], m[i + 1]["repeat"]))
            self.assertNotEqual(m[i]["arm"], m[i + 1]["arm"])

    def test_the_order_of_arms_inside_a_cell_is_randomised(self):
        firsts = {suite.build_matrix(self.s, seed)[0]["arm"] for seed in range(20)}
        self.assertEqual(firsts, {"baseline", "candidate"})
        m = suite.build_matrix(self.s, 9)
        first_of_each_cell = [m[i]["arm"] for i in range(0, len(m), 2)]
        self.assertEqual(set(first_of_each_cell), {"baseline", "candidate"})  # neither arm always goes first

    def test_the_order_of_cells_is_randomised_not_the_order_of_the_suite_file(self):
        natural = [(t["id"], r) for t in self.s["tasks"] for r in range(self.s["repeats"])]
        differing = 0
        firsts = set()
        for seed in range(12):
            m = suite.build_matrix(self.s, seed)
            cells = [(m[i]["task"], m[i]["repeat"]) for i in range(0, len(m), 2)]
            self.assertEqual(sorted(cells), sorted(natural))  # the same cells, only the order changes
            differing += cells != natural
            firsts.add(cells[0])
        self.assertGreaterEqual(differing, 11)
        self.assertGreater(len(firsts), 3)

    def test_shuffle_uses_every_permutation_of_three(self):
        import random
        seen = {tuple(suite.shuffled([1, 2, 3], random.Random(s))) for s in range(200)}
        self.assertEqual(len(seen), 6)

    def test_task_body_has_ids_only_and_fits_a_board_task(self):
        body = attempt.make_body("exp1", "baseline", "calc-add", 1, 7, "/tmp/s/suite.json", "a" * 64)
        self.assertEqual(json.loads(body), {"b": 1, "exp": "exp1", "arm": "baseline", "task": "calc-add", "rep": 1,
                                            "ord": 7, "suite": "/tmp/s/suite.json", "sha": "a" * 64})
        self.assertNotIn("\\", body)
        self.assertLess(len(body), 4096)
        for bad in ("/tmp/with\\backslash/s.json", "/tmp/new\nline/s.json", "/tmp/" + "x" * 5000):
            with self.assertRaises(suite.SuiteError):
                attempt.make_body("e", "a", "t", 0, 0, bad, "a" * 64)


class PlanCommandTests(unittest.TestCase):
    def run_bench(self, *args, **kw):
        return subprocess.run([sys.executable, BENCH, *args], capture_output=True, text=True, timeout=60, **kw)

    def test_plan_prints_the_matrix_and_the_power_warning(self):
        r = self.run_bench("plan", EXAMPLE_SUITE)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("tasks 4   arms 2   repeats 2   attempts 16", r.stdout)
        self.assertIn("baseline", r.stdout)
        self.assertIn("ok (sha256:71ef44373260)", r.stdout)
        self.assertIn("a 10-point difference between two arms is usually inconclusive", r.stdout)

    def test_plan_refuses_a_fixture_that_does_not_match_its_digest(self):
        tmp = tempfile.mkdtemp(prefix="bench-plan-")
        self.addCleanup(shutil.rmtree, tmp, True)
        shutil.copytree(os.path.dirname(EXAMPLE_SUITE), os.path.join(tmp, "ex"))
        with open(os.path.join(tmp, "ex", "fixtures", "calc", "calc.py"), "a") as f:
            f.write("# edited\n")
        r = self.run_bench("plan", os.path.join(tmp, "ex", "suite.json"))
        self.assertEqual(r.returncode, 2)
        self.assertIn("DIGEST MISMATCH", r.stdout)
        self.assertIn("fixture problem", r.stderr)

    def test_plan_refuses_a_silly_matrix(self):
        tmp = tempfile.mkdtemp(prefix="bench-plan-")
        self.addCleanup(shutil.rmtree, tmp, True)
        d = base_suite()
        d["repeats"] = 20
        d["tasks"] = [dict(copy.deepcopy(d["tasks"][0]), id="t%d" % i) for i in range(60)]
        d["arms"] = [dict(copy.deepcopy(d["arms"][0]), id="a%d" % i) for i in range(4)]
        path = os.path.join(tmp, "suite.json")
        with open(path, "w") as f:
            json.dump(d, f)
        r = self.run_bench("plan", path)
        self.assertEqual(r.returncode, 2)
        self.assertIn("the matrix has 4800 attempts", r.stderr)

    def test_digest_command(self):
        r = self.run_bench("digest", os.path.join(os.path.dirname(EXAMPLE_SUITE), "fixtures", "calc"))
        self.assertEqual(r.stdout.strip(), "sha256:71ef44373260081d06e98624c88ea75613625d798c7ae7861faa27251d71905a")

    def test_one_arm_is_said(self):
        tmp = tempfile.mkdtemp(prefix="bench-plan-")
        self.addCleanup(shutil.rmtree, tmp, True)
        shutil.copytree(os.path.dirname(EXAMPLE_SUITE), os.path.join(tmp, "ex"))
        path = os.path.join(tmp, "ex", "suite.json")
        with open(path) as f:
            d = json.load(f)
        d["arms"] = d["arms"][:1]
        with open(path, "w") as f:
            json.dump(d, f)
        self.assertIn("One arm cannot be compared", self.run_bench("plan", path).stdout)


if __name__ == "__main__":
    unittest.main()
