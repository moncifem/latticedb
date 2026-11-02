"""Shared fixtures for the console tests (stdlib unittest only)."""

from __future__ import annotations

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
if ROOT not in sys.path:
    sys.path.insert(0, ROOT)

STATIC = os.path.join(ROOT, "console", "static")


def task(tid, state="open", owner=None, parent=None, created=1000, updated=None, priority=0,
         title=None, seq=None, by="maintainer", body=""):
    return {
        "id": tid, "title": title or ("title of " + tid), "state": state, "owner": owner,
        "priority": priority, "parent_id": parent, "created_by": by, "body": body,
        "created_at": created, "updated_at": created if updated is None else updated,
        "claim_seq": 0, "done_seq": 0, "created_seq": seq if seq is not None else created,
    }


def event(eid, tid, author, kind, ts, body=""):
    return {"id": eid, "task_id": tid, "author": author, "kind": kind, "body": body,
            "created_at": ts}


def decision(did, tid, author, ts, body="because"):
    return {"id": did, "task_id": tid, "author": author, "body": body, "created_at": ts}


def lease(tid, until=0, hb=0, attempts=0, max_attempts=3, not_before=0, last_error="", blocked=""):
    return {"task_id": tid, "lease_until": until, "heartbeat_at": hb, "attempts": attempts,
            "max_attempts": max_attempts, "not_before": not_before, "last_error": last_error,
            "blocked_by": blocked}


class FakeSource:
    """Answers like PsqlSource from lists the test owns; counts the calls it gets."""

    demo = False
    label = "fake"

    clock_source = "database"

    def __init__(self, tasks=None, events=None, decisions=None, now=100000, leases=None):
        self.lease_rows = list(leases or [])
        self.task_rows = list(tasks or [])
        self.event_rows = list(events or [])
        self.decision_rows = list(decisions or [])
        self.clock_now = now
        self.calls = []
        self.error = None

    def _check(self, name):
        self.calls.append(name)
        if self.error is not None:
            raise self.error

    def now(self):
        return self.clock_now

    def tasks(self, limit):
        self._check("tasks")
        rows = self.task_rows
        return rows[-limit:], len(rows) > limit

    def leases(self, limit):
        self._check("leases")
        return list(self.lease_rows[:limit]), True

    def events_latest(self, limit):
        self._check("events_latest")
        return list(self.event_rows[-limit:]), len(self.event_rows) > limit

    def events_after(self, after, limit):
        self._check("events_after")
        return [e for e in self.event_rows if e["id"] > after][:limit]

    def decisions_latest(self, limit):
        self._check("decisions_latest")
        return list(self.decision_rows[-limit:])

    def decisions_after(self, after, limit):
        self._check("decisions_after")
        return [d for d in self.decision_rows if d["id"] > after][:limit]


def small_board():
    """Three agents, a parent with two children, one task of each state."""
    tasks = [
        task("tparent", "claimed", "alice", created=1000, seq=1, title="Parent"),
        task("tchild1", "done", "bob", parent="tparent", created=1100, updated=2200, seq=2),
        task("tchild2", "open", None, parent="tparent", created=1200, seq=3),
    ]
    events = [
        event(1, "tparent", "maint", "add", 1000, "Parent"),
        event(2, "tchild1", "maint", "add", 1100, "child one"),
        event(3, "tchild2", "maint", "add", 1200, "child two"),
        event(4, "tparent", "alice", "claim", 1300),
        event(5, "tchild1", "bob", "claim", 1400),
        event(6, "tchild1", "bob", "note", 1500, "halfway"),
        event(7, "tchild1", "bob", "decision", 1600, "use the small one"),
        event(8, "tchild1", "bob", "done", 2200, "finished"),
    ]
    decisions = [decision(7, "tchild1", "bob", 1600, "use the small one")]
    return tasks, events, decisions
