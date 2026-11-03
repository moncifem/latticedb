"""Shared plumbing of the board bench tests.

BENCH_UNDER_TEST names another copy of integrations/board (test_bench_mutations.py plants one fault at a
time in a copy and runs these tests against it). Everything the tests import or run comes from there.
"""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REAL_BOARD_DIR = os.path.dirname(HERE)
BOARD_DIR = os.environ.get("BENCH_UNDER_TEST") or REAL_BOARD_DIR
if BOARD_DIR not in sys.path:
    sys.path.insert(0, BOARD_DIR)
BOARD = os.path.join(BOARD_DIR, "board")
BENCH = os.path.join(BOARD_DIR, "board-bench")
EXAMPLE_SUITE = os.path.join(BOARD_DIR, "bench", "examples", "suite.json")


def att(arm, task, rep, outcome="pass", **kw):
    """An attempt row in the shape benchlib.report.parse_rows returns."""
    row = {"board_task_id": "t%s%s%d" % (arm, task, rep), "arm": arm, "task_key": task, "repeat": rep,
           "order": 0, "outcome": outcome, "flaky": False, "tampered": 0, "duration_ms": None, "cost_usd": None,
           "tokens": None, "grader_exit": None, "grader_runs": "", "harness": "board-bench/1",
           "recorded_at": 1, "summary": ""}
    row.update(kw)
    return row


def data_of(attempts, arms=None, repeats=2, name="e1"):
    arms = arms or sorted({a["arm"] for a in attempts})
    tasks = sorted({a["task_key"] for a in attempts})
    return {"experiment": {"name": name, "suite": "s", "suite_sha": "0" * 64, "seed": 1, "repeats": repeats,
                           "planned": len(attempts), "harness": "board-bench/1", "created_by": "x", "created_at": 1},
            "arms": [{"arm": a, "agent": "cmd", "config_sha": "0" * 64} for a in arms],
            "tasks": [{"task": t, "fixture_digest": "0" * 64} for t in tasks],
            "attempts": attempts, "truncated": False}
