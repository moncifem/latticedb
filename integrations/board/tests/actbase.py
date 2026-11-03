"""Shared plumbing of the live-activity tests: the board helper driven against a fake psql with a fake clock.

BOARD_UNDER_TEST names another copy of the helper (test_activity_mutations.py runs the same tests against
copies with one rule removed); ACTIVITY_HOOK_UNDER_TEST does the same for hooks/activity-hook.py.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
BOARD = os.environ.get("BOARD_UNDER_TEST") or os.path.join(BOARD_DIR, "board")
HOOK = os.environ.get("ACTIVITY_HOOK_UNDER_TEST") or os.path.join(BOARD_DIR, "hooks", "activity-hook.py")
FAKE = os.path.join(HERE, "fakepsql.py")

WRITE = r"INSERT INTO agent_activity_log"
RING_DELETE = r"DELETE FROM agent_activity_log WHERE agent"
T0 = 1_000_000


def vars_of(call):
    """The -v name=value pairs of one recorded psql call."""
    out = {}
    argv = call["argv"]
    for i, a in enumerate(argv):
        if a == "-v" and i + 1 < len(argv) and "=" in argv[i + 1]:
            k, v = argv[i + 1].split("=", 1)
            out[k] = v
    return out


class ActCli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="board-act-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        os.chmod(self.tmp, 0o700)
        self.log = os.path.join(self.tmp, "calls.jsonl")
        self.rules_path = os.path.join(self.tmp, "rules.json")
        self.state = os.path.join(self.tmp, "state")
        self.psql = os.path.join(self.tmp, "psql")
        with open(self.psql, "w") as f:
            f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, FAKE))
        os.chmod(self.psql, 0o755)
        # A write that succeeds and says the session row exists.
        self.ok_rows = [{"match": WRITE, "out": "row\talice/main"}]
        self.rules = list(self.ok_rows)

    def env(self, now=None, author="alice", activity="1", extra=None):
        e = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "TMPDIR": self.tmp,
             "BOARD_PSQL": self.psql, "BOARD_STATE_DIR": self.state, "FAKE_LOG": self.log,
             "FAKE_RULES": self.rules_path, "BOARD_TEST_MODE": "1", "BOARD_ACTIVITY_NOW": str(T0 if now is None else now)}
        if author:
            e["BOARD_AUTHOR"] = author
        if activity is not None:
            e["BOARD_ACTIVITY"] = activity
        e.update(extra or {})
        return e

    def board(self, *args, now=None, author="alice", activity="1", extra=None, rules=None):
        if rules is not None:
            self.rules = rules
        with open(self.rules_path, "w") as f:
            json.dump(self.rules, f)
        return subprocess.run(["sh", BOARD, *args], env=self.env(now, author, activity, extra),
                              capture_output=True, text=True, timeout=60)

    def act(self, tool="Edit", *more, now=None, **kw):
        return self.board("activity", "--tool", tool, *more, now=now, **kw)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [json.loads(line) for line in f if line.strip()]

    def matching(self, pattern):
        import re
        return [c for c in self.calls() if re.search(pattern, c["sql"], re.S)]

    def writes(self):
        return self.matching(WRITE)

    def activity_dir(self):
        return os.path.join(self.state, "activity")
