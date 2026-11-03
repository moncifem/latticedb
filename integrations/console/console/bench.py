"""The Experiments screen's data: the report of `board bench`, computed by the same code the command line
uses (integrations/board/benchlib), so the page and `board-bench report` cannot disagree.

Read only: the source answers a few SELECT statements (console/source.py) and everything else is arithmetic.
"""

from __future__ import annotations

import os
import sys
from typing import Optional

_BOARD_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "board")

try:
    if os.path.isdir(os.path.join(_BOARD_DIR, "benchlib")) and _BOARD_DIR not in sys.path:
        sys.path.insert(0, _BOARD_DIR)
    from benchlib import report as _report
except ImportError:  # the console was copied away from integrations/board
    _report = None

MAX_NAME = 48


def available() -> bool:
    return _report is not None


def unavailable_payload(meta: dict) -> dict:
    return {"meta": meta, "available": False, "experiments": [],
            "note": "The experiment screen needs integrations/board/benchlib next to the console."}


def list_payload(source, meta: dict) -> dict:
    if _report is None:
        return unavailable_payload(meta)
    rows, ok = source.bench_experiments()
    exps = [{"name": r["id"], "suite": r["suite"], "repeats": r["repeats"], "planned": r["planned"],
             "seed": r["seed"], "harness": r["harness_version"], "created_by": r["created_by"],
             "created_at": r["created_at"]} for r in rows]
    return {"meta": meta, "available": ok, "experiments": exps,
            "note": ("" if ok else "This board has no experiment tables yet: run `board init` (or `board migrate`).")}


def experiment_payload(source, name: str, meta: dict, rule: str = "majority") -> Optional[dict]:
    if _report is None:
        return None
    data = source.bench_data(name)
    if data is None:
        return None
    rep = _report.build_report(data, rule)
    rep["meta"] = meta
    return rep
