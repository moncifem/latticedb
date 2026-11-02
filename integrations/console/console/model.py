"""Derive agents, attention items, task trees and per-task facts from board rows.

Rows are plain dicts with the board's column names (integrations/board/schema.sql). All
functions are pure: `now` is passed in. Text in the rows is written by agents and is untrusted;
nothing here interprets it.
"""

from __future__ import annotations

import json
from bisect import bisect_right
from dataclasses import dataclass, field
from typing import Optional

MAX_CHAIN = 50  # a parent chain longer than this is reported as too deep, not followed
TASK_STATES = ("open", "claimed", "done")
EVENT_KINDS = ("add", "claim", "note", "done", "decision")
MAX_TEXT = 4096  # the helper's limit; direct writers may exceed it, the API cuts there
TASK_EVENT_LIMIT = 500  # events (and decisions) one task answer carries, newest first
TASK_CHILD_LIMIT = 200
EVENT_BUDGET = 2 * 1024 * 1024  # JSON bytes of events in one answer; the whole answer stays under 4 MiB
DECISION_BUDGET = 512 * 1024
FEED_BUDGET = 3 * 1024 * 1024


def json_cost(row) -> int:
    """Bytes this row adds to a JSON answer (ASCII-escaped, so one non-BMP character is 12)."""
    return len(json.dumps(row, ensure_ascii=True, separators=(",", ":"))) + 1


def newest_that_fit(rows: list, limit: int, budget: int) -> tuple:
    """The newest rows (input is oldest first) within limit and budget, still oldest first, and
    whether any were left out. At least one row is always kept."""
    kept, used = [], 0
    for r in reversed(rows):
        cost = json_cost(r)
        if kept and (len(kept) >= limit or used + cost > budget):
            break
        kept.append(r)
        used += cost
    kept.reverse()
    return kept, len(kept) < len(rows)


def clip(text, limit: int = MAX_TEXT) -> str:
    s = "" if text is None else str(text)
    return s if len(s) <= limit else s[:limit]


@dataclass
class TaskFact:
    """What the metrics need to know about one task."""

    task: dict
    claims: list = field(default_factory=list)  # (author, created_at, event_id), by event id
    first_claim_ts: Optional[int] = None
    done_ts: Optional[int] = None
    notes: int = 0
    decisions: int = 0
    last_activity: int = 0
    claimant: Optional[str] = None


def task_facts(tasks: list, events: list, decisions: list) -> list:
    """One TaskFact per task. Events must be sorted by id (ascending)."""
    by_task: dict = {}
    for e in events:
        by_task.setdefault(e["task_id"], []).append(e)
    dec_count: dict = {}
    for d in decisions:
        dec_count[d["task_id"]] = dec_count.get(d["task_id"], 0) + 1
    out = []
    for t in tasks:
        evs = by_task.get(t["id"], [])
        f = TaskFact(task=t)
        last = max(t["updated_at"], t["created_at"])
        for e in evs:
            last = max(last, e["created_at"])
            if e["kind"] == "claim":
                f.claims.append((e["author"], e["created_at"], e["id"]))
            elif e["kind"] == "done" and f.done_ts is None:
                f.done_ts = e["created_at"]
            elif e["kind"] == "note":
                f.notes += 1
        f.decisions = dec_count.get(t["id"], 0)
        if f.claims:
            f.first_claim_ts = f.claims[0][1]
            f.claimant = f.claims[0][0]
        elif t.get("owner"):
            f.claimant = t["owner"]  # the claim event may lie outside the loaded window
        f.last_activity = last
        out.append(f)
    return out


def counts_by_state(tasks: list) -> dict:
    c = {s: 0 for s in TASK_STATES}
    c["other"] = 0
    for t in tasks:
        s = t["state"]
        c[s if s in TASK_STATES else "other"] += 1
    return c


def build_agents(tasks: list, events: list, facts: list, now: int, stale_s: int) -> list:
    """Agents are every event author and every task owner. State:
    working = holds a claimed task with activity inside the stale window;
    stale   = holds claimed tasks and none has activity inside the window;
    idle    = holds no claimed task."""
    names: set = set()
    last_event: dict = {}
    event_count: dict = {}
    for e in events:
        names.add(e["author"])
        event_count[e["author"]] = event_count.get(e["author"], 0) + 1
        cur = last_event.get(e["author"])
        if cur is None or e["id"] > cur["id"]:
            last_event[e["author"]] = e
    for t in tasks:
        if t.get("owner"):
            names.add(t["owner"])
    fact_by_id = {f.task["id"]: f for f in facts}
    done_by: dict = {}
    for e in events:
        if e["kind"] == "done":
            done_by.setdefault(e["author"], set()).add(e["task_id"])
    held: dict = {}
    for t in tasks:
        if t["state"] == "claimed" and t.get("owner"):
            held.setdefault(t["owner"], []).append(t)
    agents = []
    for name in sorted(names):
        mine = held.get(name, [])
        live = [t for t in mine if now - fact_by_id[t["id"]].last_activity <= stale_s]
        if live:
            state = "working"
        elif mine:
            state = "stale"
        else:
            state = "idle"
        focus = None
        pool = live or mine
        if pool:
            focus = max(pool, key=lambda t: (fact_by_id[t["id"]].last_activity, t["id"]))
        le = last_event.get(name)
        agents.append(
            {
                "name": name,
                "state": state,
                "current_task": ({"id": focus["id"], "title": clip(focus["title"], 300)}
                                 if focus else None),
                "claimed": sorted(t["id"] for t in mine),
                "last_event": ({"id": le["id"], "kind": le["kind"], "task_id": le["task_id"],
                                "created_at": le["created_at"],
                                "age_s": max(0, now - le["created_at"])} if le else None),
                "events": event_count.get(name, 0),
                "tasks_done": len(done_by.get(name, ())),
            }
        )
    order = {"stale": 0, "working": 1, "idle": 2}
    agents.sort(key=lambda a: (order[a["state"]], a["name"]))
    return agents


def attention(tasks: list, facts: list, now: int, stale_s: int, open_s: int) -> dict:
    """The needs-attention strip. Every item names the rule that put it there."""
    stale, mismatch, double, open_long = [], [], [], []
    for f in facts:
        t = f.task
        if t["state"] == "claimed":
            idle = now - f.last_activity
            if idle > stale_s:
                stale.append({"task_id": t["id"], "title": clip(t["title"], 300),
                              "owner": t.get("owner"), "idle_s": max(0, idle)})
            winner = f.claims[-1][0] if f.claims else None
            if winner and t.get("owner") and winner != t["owner"]:
                mismatch.append({"task_id": t["id"], "title": clip(t["title"], 300),
                                 "owner": t["owner"], "last_claim_by": winner})
        authors = []
        for a, _, _ in f.claims:
            if a not in authors:
                authors.append(a)
        if len(authors) >= 2:
            double.append({"task_id": t["id"], "title": clip(t["title"], 300),
                           "claimants": authors})
        if t["state"] == "open" and now - t["created_at"] > open_s:
            open_long.append({"task_id": t["id"], "title": clip(t["title"], 300),
                              "age_s": max(0, now - t["created_at"])})
    stale.sort(key=lambda x: -x["idle_s"])
    open_long.sort(key=lambda x: -x["age_s"])
    return {
        "stale": stale,
        "double_claims": double,
        "owner_mismatch": mismatch,
        "open_too_long": open_long,
        "rules": {
            "stale": "claimed task with no event or update for more than %d minutes" % (stale_s // 60),
            "double_claims": "task with claim events from two or more authors (the helper writes "
                             "only the winning claim, so this needs a direct write)",
            "owner_mismatch": "claimed task whose owner differs from the author of its last claim event",
            "open_too_long": "open task created more than %d hours ago" % (open_s // 3600),
        },
    }


def parent_chain(task_id: str, by_id: dict) -> dict:
    """Ancestors of task_id, root first, never looping. `issue` is None, 'cycle',
    'missing_parent' (the parent id names no task) or 'too_deep'."""
    chain: list = []
    seen = {task_id}
    issue = None
    cur = by_id.get(task_id)
    while cur is not None:
        pid = cur.get("parent_id")
        if not pid:
            break
        if pid in seen:
            issue = "cycle"
            break
        if len(chain) >= MAX_CHAIN:
            issue = "too_deep"
            break
        parent = by_id.get(pid)
        if parent is None:
            chain.append({"id": pid, "title": None, "state": None, "missing": True})
            issue = "missing_parent"
            break
        seen.add(pid)
        chain.append({"id": parent["id"], "title": clip(parent["title"], 300),
                      "state": parent["state"], "missing": False})
        cur = parent
    chain.reverse()
    return {"chain": chain, "issue": issue}


def children_of(task_id: str, tasks: list) -> list:
    kids = [t for t in tasks if t.get("parent_id") == task_id and t["id"] != task_id]
    kids.sort(key=lambda t: (-t["priority"], t["created_seq"], t["id"]))
    return [{"id": t["id"], "title": clip(t["title"], 300), "state": t["state"],
             "owner": t.get("owner")} for t in kids]


def task_view(task_id: str, tasks: list, events: list, decisions: list, now: int) -> Optional[dict]:
    by_id = {t["id"]: t for t in tasks}
    t = by_id.get(task_id)
    if t is None:
        return None
    pc = parent_chain(task_id, by_id)
    evs = [dict(e, body=clip(e["body"])) for e in events if e["task_id"] == task_id]
    decs = [dict(d, body=clip(d["body"])) for d in decisions if d["task_id"] == task_id]
    kids = children_of(task_id, tasks)
    kept_e, cut_e = newest_that_fit(evs, TASK_EVENT_LIMIT, EVENT_BUDGET)
    kept_d, cut_d = newest_that_fit(decs, TASK_EVENT_LIMIT, DECISION_BUDGET)
    return {
        "task": {k: (clip(v) if isinstance(v, str) else v) for k, v in t.items()},
        "parents": pc["chain"],
        "parent_issue": pc["issue"],
        "children": kids[:TASK_CHILD_LIMIT],
        "children_total": len(kids),
        "events": kept_e,
        "events_total": len(evs),
        "events_truncated": cut_e,
        "decisions": kept_d,
        "decisions_total": len(decs),
        "decisions_truncated": cut_d,
        "now": now,
    }


def _row(e: dict, tasks_by_id: dict) -> dict:
    t = tasks_by_id.get(e["task_id"])
    return dict(e, body=clip(e["body"]), task_title=clip(t["title"], 300) if t else None)


def _matches(e: dict, agent, kind, task) -> bool:
    return not ((agent and e["author"] != agent) or (kind and e["kind"] != kind)
                or (task and e["task_id"] != task))


def feed_page(events: list, tasks_by_id: dict, after: Optional[int], before: Optional[int],
              limit: int, agent: Optional[str], kind: Optional[str], task: Optional[str],
              budget: Optional[int] = None, info: Optional[dict] = None) -> list:
    """Matching events, newest first (the first page, and paging back). `after` keeps id > after,
    `before` keeps id < before. With a budget the page shrinks to fit it, and info["cut"] says
    that matching rows were left out. To follow new events use feed_after."""
    out, used = [], 0
    if info is not None:
        info["cut"] = False
    for e in reversed(events):
        if after is not None and e["id"] <= after:
            break  # events are sorted by id, so everything earlier is older still
        if before is not None and e["id"] >= before:
            continue
        if not _matches(e, agent, kind, task):
            continue
        row = _row(e, tasks_by_id)
        if budget is not None:
            cost = json_cost(row)
            if out and used + cost > budget:
                if info is not None:
                    info["cut"] = True
                break
            used += cost
        out.append(row)
        if len(out) >= limit:
            break
    return out


def feed_after(events: list, tasks_by_id: dict, after: int, limit: int, agent, kind, task,
               budget: int) -> tuple:
    """Follow the feed: matching events with id > after, taken oldest first until the limit or the
    budget, returned newest first together with the cursor to ask from next time. If rows were
    left out the cursor stops just before the first of them, so none is skipped, and it is always
    past the rows returned. Otherwise it is the id of the newest event, matching or not."""
    ids = [e["id"] for e in events]
    out, used = [], 0
    cursor = events[-1]["id"] if events else after
    for e in events[bisect_right(ids, after):]:
        if not _matches(e, agent, kind, task):
            continue
        row = _row(e, tasks_by_id)
        cost = json_cost(row)
        if out and (len(out) >= limit or used + cost > budget):
            cursor = e["id"] - 1
            break
        out.append(row)
        used += cost
    out.reverse()
    return out, max(cursor, after)
