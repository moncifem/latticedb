#!/usr/bin/env python3
"""A grader that disagrees with itself, on purpose: it runs the tests like the others, but passes only on
its odd-numbered runs (a counter file in the work directory). With `reruns: 2` the runs are pass, fail,
pass: the attempt passes by majority and is marked flaky."""

import os
import subprocess
import sys

work = os.environ["BENCH_WORKDIR"]
counter = os.path.join(work, ".flaky-count")
n = int(open(counter).read()) + 1 if os.path.exists(counter) else 1
with open(counter, "w") as f:
    f.write(str(n))
rc = subprocess.call([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=work)
sys.exit(rc if n % 2 == 1 else 1)
