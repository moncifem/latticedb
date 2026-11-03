"""The agents quickstart must not name a command, option or file that does not exist.

It cannot prove the steps work (that took a local dev stack, and the dev-stack job runs the swarm and
test.sh); it catches drift: a renamed subcommand or option, a moved script.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import os
import re
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(BOARD_DIR))
QUICKSTART = os.path.join(ROOT, "docs", "quickstart-agents.md")


def code_lines():
    with open(QUICKSTART, encoding="utf-8") as f:
        text = f.read()
    lines = []
    for block in re.findall(r"```sh\n(.*?)```", text, re.S):
        joined = block.replace("\\\n", " ")
        lines += [ln.strip() for ln in joined.splitlines() if ln.strip() and not ln.strip().startswith("#")]
    return lines


class Quickstart(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.lines = code_lines()
        cls.run_help = subprocess.run([sys.executable, os.path.join(BOARD_DIR, "board-run"), "--help"],
                                      capture_output=True, text=True, timeout=30).stdout
        cls.usage = subprocess.run(["sh", os.path.join(BOARD_DIR, "board"), "help"], capture_output=True,
                                   text=True, timeout=30).stdout
        cls.swarm_help = subprocess.run([sys.executable, os.path.join(BOARD_DIR, "swarm"), "--help"],
                                        capture_output=True, text=True, timeout=30).stdout

    def test_it_has_the_commands_it_describes(self):
        self.assertGreater(len([ln for ln in self.lines if ln.startswith("board ")]), 5)

    def test_every_board_subcommand_exists(self):
        for ln in self.lines:
            m = re.match(r"board (\w+)", ln)
            if m:
                self.assertRegex(self.usage, r"(?m)^  %s\b" % m.group(1), ln)

    def test_every_option_of_board_run_exists(self):
        for ln in self.lines:
            if ln.startswith("board run"):
                for opt in re.findall(r"(--[a-z][a-z-]*)", ln.split(" -- ")[0]):
                    self.assertIn(opt, self.run_help, ln)

    def test_every_option_of_the_swarm_exists(self):
        for ln in self.lines:
            if "integrations/board/swarm" in ln:
                for opt in re.findall(r"(--[a-z][a-z-]*)", ln):
                    self.assertIn(opt, self.swarm_help, ln)

    def test_every_option_of_board_add_exists_in_the_usage(self):
        for ln in self.lines:
            if ln.startswith("board add"):
                for opt in re.findall(r"(--[a-z][a-z-]*)", ln):
                    self.assertIn(opt, self.usage, ln)

    def test_every_board_bench_subcommand_and_option_exists(self):
        help_of = {}
        top = subprocess.run([sys.executable, os.path.join(BOARD_DIR, "board-bench"), "--help"], capture_output=True,
                             text=True, timeout=30).stdout
        checked = 0
        for ln in self.lines:
            m = re.match(r"\$BENCH (\w[\w-]*)(.*)", ln)
            if not m:
                continue
            sub, rest = m.group(1), m.group(2)
            self.assertIn(sub, top, ln)
            if sub not in help_of:
                help_of[sub] = subprocess.run([sys.executable, os.path.join(BOARD_DIR, "board-bench"), sub, "--help"],
                                              capture_output=True, text=True, timeout=30).stdout
            for opt in re.findall(r"(--[a-z][a-z-]*)", rest):
                self.assertIn(opt, help_of[sub], ln)
            checked += 1
        self.assertGreaterEqual(checked, 5)  # plan, start, approve, report (twice)

    def test_the_bench_section_runs_the_example_suite_that_exists(self):
        with open(QUICKSTART, encoding="utf-8") as f:
            text = f.read()
        self.assertIn("integrations/board/bench/examples", text)
        self.assertTrue(os.path.exists(os.path.join(BOARD_DIR, "bench", "examples", "suite.json")))
        self.assertTrue(os.path.exists(os.path.join(BOARD_DIR, "bench", "README.md")))

    def test_the_programs_it_runs_exist(self):
        for rel in ("integrations/console/lattice-console", "integrations/board/swarm", "dev/lattice-dev.sh",
                    "integrations/board/board", "integrations/board/board-run", "integrations/board/board-bench"):
            self.assertTrue(os.access(os.path.join(ROOT, rel), os.X_OK), rel)
        with open(QUICKSTART, encoding="utf-8") as f:
            text = f.read()
        for rel in re.findall(r"\]\((\.\./[^)#]+|[a-z][^):]*\.md)(?:#[^)]*)?\)", text):
            self.assertTrue(os.path.exists(os.path.normpath(os.path.join(ROOT, "docs", rel))), rel)


if __name__ == "__main__":
    unittest.main()
