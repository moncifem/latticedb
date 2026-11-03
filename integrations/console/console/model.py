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
TASK_STATES = ("open", "claimed", "done", "dead", "cancelled", "awaiting_approval")
EVENT_KINDS = ("add", "claim", "note", "done", "decision", "fail", "expired", "release", "approve",
               "cancel", "block", "unblock", "fenced", "reject", "requeue", "priority")
QUEUE_LIST_LIMIT = 200  # rows per queue section; the section also reports its total
BLOCKER_WALK_LIMIT = 256  # tasks visited when looking for a dependency cycle
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


# ---------------------------------------------------------------- the queue (leases, retries, deps)

LEASE_DEFAULT = {"lease_until": 0, "heartbeat_at": 0, "attempts": 0, "max_attempts": 3,
                 "not_before": 0, "last_error": "", "blocked_by": ""}


def split_blockers(text) -> list:
    """The ids in a blocked_by text (comma separated), in order, without repeats or blanks."""
    out: list = []
    for part in ("" if text is None else str(text)).split(","):
        part = part.strip()
        if part and part not in out:
            out.append(part)
    return out[:64]


def lease_of(leases_by_id: dict, task_id: str) -> dict:
    """A task's lease row, or the defaults a task without a row has (made by an older helper)."""
    return leases_by_id.get(task_id) or LEASE_DEFAULT


def _in_cycle(task_id: str, waits: dict) -> bool:
    """True when task_id can reach itself through the blocked_by lists in `waits` (bounded)."""
    seen: set = set()
    stack = list(waits.get(task_id, ()))
    visited = 0
    while stack and visited < BLOCKER_WALK_LIMIT:
        cur = stack.pop()
        if cur == task_id:
            return True
        if cur in seen:
            continue
        seen.add(cur)
        visited += 1
        stack.extend(waits.get(cur, ()))
    return False


def _short(t: dict) -> dict:
    return {"id": t["id"], "title": clip(t["title"], 300), "priority": t["priority"]}


def build_queue(tasks: list, leases: list, now: int, leases_available: bool = True,
                tasks_truncated: bool = False) -> dict:
    """Sort every task into one queue section, from the task rows and the lease rows.

    runnable   open, past its retry delay, every blocker done (what `board next` would offer)
    blocked    open with a blocker that is not done; `forever` when a blocker is dead, cancelled,
               missing or the task waits (through others) for itself
    claimed    held under a lease; `expired` when lease_until has passed and nobody reaped it yet
    retrying   open, failed or expired before, still inside its retry delay
    dead       out of attempts
    awaiting   waiting for board approve
    All times are database-clock seconds and `now` must be on that clock. When the task window was
    cut (tasks_truncated), a blocker id that is not in it is `not_in_window`: unknown, not missing,
    and it does not mark the task as blocked for ever."""
    by_id = {t["id"]: t for t in tasks}
    lease_by_id = {r["task_id"]: r for r in leases}
    waits = {t["id"]: split_blockers(lease_of(lease_by_id, t["id"])["blocked_by"]) for t in tasks}
    sections: dict = {k: [] for k in ("runnable", "blocked", "claimed", "retrying", "dead", "awaiting")}
    for t in sorted(tasks, key=lambda x: (-x["priority"], x["created_seq"], x["id"])):
        lease = lease_of(lease_by_id, t["id"])
        base = dict(_short(t), attempts=lease["attempts"], max_attempts=lease["max_attempts"])
        state = t["state"]
        if state == "open":
            blockers = []
            for b in waits[t["id"]]:
                bt = by_id.get(b)
                bstate = bt["state"] if bt else None
                if bstate == "done":
                    continue
                problem = None
                if bt is None:
                    problem = "not_in_window" if tasks_truncated else "missing"
                elif bstate in ("dead", "cancelled"):
                    problem = bstate
                blockers.append({"id": b, "state": bstate, "title": clip(bt["title"], 300) if bt else None,
                                 "problem": problem})
            cyc = bool(blockers) and _in_cycle(t["id"], waits)
            if blockers:
                forever = cyc or any(b["problem"] in ("dead", "cancelled", "missing") for b in blockers)
                sections["blocked"].append(dict(base, blockers=blockers, forever=forever, cycle=cyc,
                                                age_s=max(0, now - t["created_at"])))
            elif lease["not_before"] > now:
                sections["retrying"].append(dict(base, not_before=lease["not_before"],
                                                 wait_s=lease["not_before"] - now,
                                                 last_error=clip(lease["last_error"], 300)))
            else:
                sections["runnable"].append(dict(base, age_s=max(0, now - t["created_at"]),
                                                 last_error=clip(lease["last_error"], 300)))
        elif state == "claimed":
            until = lease["lease_until"]
            hb = lease["heartbeat_at"]
            sections["claimed"].append(dict(
                base, owner=t.get("owner"), lease_until=until,
                remaining_s=(until - now) if until else None,
                expired=bool(until) and until < now,
                heartbeat_at=hb, heartbeat_age_s=(max(0, now - hb) if hb else None),
                has_lease=bool(until)))
        elif state == "dead":
            sections["dead"].append(dict(base, last_error=clip(lease["last_error"], 300),
                                         updated_at=t["updated_at"]))
        elif state == "awaiting_approval":
            sections["awaiting"].append(dict(base, age_s=max(0, now - t["created_at"])))
    totals = {k: len(v) for k, v in sections.items()}
    out = {k: v[:QUEUE_LIST_LIMIT] for k, v in sections.items()}
    # most urgent first inside the sections where order carries meaning
    out["claimed"].sort(key=lambda r: (r["remaining_s"] is None, r["remaining_s"] if r["remaining_s"] is not None else 0))
    out["retrying"].sort(key=lambda r: r["not_before"])
    out["totals"] = totals
    out["truncated"] = {k: totals[k] > QUEUE_LIST_LIMIT for k in totals}
    out["leases_available"] = leases_available
    out["now"] = now
    return out


def queue_attention(queue: dict) -> dict:
    """The queue's contribution to the needs-attention strip, from build_queue's answer."""
    expired = [{"task_id": r["id"], "title": r["title"], "owner": r["owner"],
                "overdue_s": max(0, -r["remaining_s"])} for r in queue["claimed"] if r["expired"]]
    dead = [{"task_id": r["id"], "title": r["title"], "attempts": r["attempts"],
             "max_attempts": r["max_attempts"], "last_error": r["last_error"]} for r in queue["dead"]]
    forever = [{"task_id": r["id"], "title": r["title"],
                "reasons": [b["id"] + " (%s)" % b["problem"]
                            for b in r["blockers"] if b["problem"] in ("dead", "cancelled", "missing")] + (["a cycle"] if r["cycle"] else [])}
               for r in queue["blocked"] if r["forever"]]
    expired.sort(key=lambda x: -x["overdue_s"])
    return {
        "expired_leases": expired,
        "dead_tasks": dead,
        "blocked_forever": forever,
        "queue_rules": {
            "expired_leases": "claimed task whose lease ran out and that nobody has reaped yet "
                              "(any agent's `board reap` or `board next` expires it)",
            "dead_tasks": "task that used all its attempts; it needs a person",
            "blocked_forever": "open task waiting for a blocker that is dead, cancelled, missing, "
                               "or for a cycle: it can never become runnable",
        },
    }
