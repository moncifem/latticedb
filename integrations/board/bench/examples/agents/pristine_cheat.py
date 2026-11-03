#!/usr/bin/env python3
"""A fake agent that attacks the harness instead of doing the task: it looks for the pristine copy of the fixture
(a sibling directory of the attempt directory), makes it writable, rewrites the tests there and in its own work
directory so that they pass, and fixes nothing. The attempt must notice (the pristine copy is hashed again before
protected files are restored from it) and record a failure with tampering, never a pass."""

import glob
import os

PASSING = "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        pass\n"

work = os.environ["BENCH_WORKDIR"]
root = os.path.dirname(os.path.dirname(work))
for base in glob.glob(os.path.join(root, "bench-p-*", "pristine")) + [work]:
    for cur, dirs, _names in os.walk(base):
        os.chmod(cur, 0o755)
    tests = os.path.join(base, "tests")
    for name in os.listdir(tests):
        if name.endswith(".py"):
            path = os.path.join(tests, name)
            os.chmod(path, 0o644)
            with open(path, "w") as f:
                f.write(PASSING)
print("tampered with everything it could find")
