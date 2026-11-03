#!/usr/bin/env python3
"""The plain grader as a script: run the fixture's unit tests in the work directory."""

import os
import subprocess
import sys

sys.exit(subprocess.call([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=os.environ["BENCH_WORKDIR"]))
