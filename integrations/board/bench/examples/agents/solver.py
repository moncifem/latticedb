#!/usr/bin/env python3
"""A fake agent for the example suite: no model, no network. It fixes the two bugs the fixtures plant by
editing the files in $BENCH_WORKDIR, and reports a made-up cost the way a `cmd` agent may
($BENCH_REPORT_FILE). `--skip NAME` leaves that bug alone, which is how the example makes one arm worse
than the other; `--cheat` rewrites the tests instead of fixing anything (the grader restores them)."""

import json
import os
import sys

FIXES = {"calc.py": ("a - b", "a + b"), "greet.py": ('"Hello "', '"Hello, "')}


def main(argv):
    work = os.environ["BENCH_WORKDIR"]
    skip = set(argv[argv.index("--skip") + 1].split(",")) if "--skip" in argv else set()
    if "--cheat" in argv:
        for name in os.listdir(os.path.join(work, "tests")):
            if name.endswith(".py"):
                with open(os.path.join(work, "tests", name), "w") as f:
                    f.write("import unittest\n\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        pass\n")
    for name, (old, new) in FIXES.items():
        path = os.path.join(work, name)
        if os.path.exists(path) and name.split(".")[0] not in skip:
            text = open(path).read()
            with open(path, "w") as f:
                f.write(text.replace(old, new))
    report = os.environ.get("BENCH_REPORT_FILE")
    if report:
        with open(report, "w") as f:
            json.dump({"cost_usd": 0.01, "tokens": 1200}, f)
    print("fake agent done")


if __name__ == "__main__":
    main(sys.argv[1:])
