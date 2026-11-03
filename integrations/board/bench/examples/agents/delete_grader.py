#!/usr/bin/env python3
"""A fake agent that deletes the grader's script (the path is its first argument), which an agent running as the
same user can do. The attempt hashes pinned grader files before every grader run, so the result is a failure with
tampering recorded, not a harness error that would drop the attempt from the arm's rates."""

import os
import sys

try:
    os.unlink(sys.argv[1])
except OSError:
    pass
print("deleted the grader")
