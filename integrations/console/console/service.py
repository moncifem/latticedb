"""Caches the board in memory and answers the console's questions from it.

The cache is fed by the source: tasks are re-read on every refresh (the board has a few thousand
at most); events and decisions are read once, newest window first, then only "id greater than the
last one seen", which is the cursor rule of integrations/board/CONVENTIONS.md. A refresh is at
most once per `ttl` seconds however many browsers poll.
"""

from __future__ import annotations

import threading
import time
from collections import OrderedDict
from dataclasses import dataclass, field
from typing import Optional

from . import bench, metrics, model

TASK_LIMIT = 5000
EVENT_WINDOW = 20000
DECISION_WINDOW = 5000
PAGE = 2000
MAX_PAGES_PER_REFRESH = 10
FAIL_BACKOFF_S = 2.0  # after a failed read, answer from the failure instead of starting psql again
SCORECARD_CACHE = 16
BENCH_CACHE = 8
BENCH_TTL_S = 5.0  # an experiment report costs a bootstrap per pair of arms; reuse it for a few seconds
WINDOW_ROUND_S = 60  # scorecard window edges are rounded to a minute so varying them cannot flood the CPU


@dataclass
class Snapshot:
    version: int = 0
    tasks: list = field(default_factory=list)
    events: list = field(default_factory=list)
    decisions: list = field(default_factory=list)
    now: int = 0
    truncated_tasks: bool = False
    truncated_events: bool = False
    truncated_decisions: bool = False
    leases: list = field(default_factory=list)
    leases_available: bool = False
    clock: str = "database"
    _queue: Optional[dict] = None
    _facts: Optional[list] = None
    _by_id: Optional[dict] = None

    @property
    def by_id(self) -> dict:
        if self._by_id is None:
            self._by_id = {t["id"]: t for t in self.tasks}
        return self._by_id

    @property
    def facts(self) -> list:
        if self._facts is None:
            self._facts = model.task_facts(self.tasks, self.events, self.decisions)
        return self._facts

    @property
    def queue(self) -> dict:
        if self._queue is None:
            self._queue = model.build_queue(self.tasks, self.leases, self.now, self.leases_available,
                                            self.truncated_tasks)
        return self._queue

    @property
    def head(self) -> int:
        return self.events[-1]["id"] if self.events else 0


def _round_window(w: tuple) -> tuple:
    r = WINDOW_ROUND_S
    return tuple(None if x is None else x // r * r for x in w)


class BoardService:
    def __init__(self, source, stale_minutes: int = 30, open_hours: int = 24, ttl: float = 1.0,
                 clock=time.monotonic):
        if not 1 <= stale_minutes <= 60 * 24 * 30:
            raise ValueError("stale_minutes must be between 1 and 43200")
        if not 1 <= open_hours <= 24 * 365:
            raise ValueError("open_hours must be between 1 and 8760")
        self.source = source
        self.stale_s = stale_minutes * 60
        self.open_s = open_hours * 3600
        self.ttl = ttl
        self.clock = clock
        self._lock = threading.Lock()
        self._snap = Snapshot()
        self._loaded = False
        self._last_refresh: Optional[float] = None
        self._cache: OrderedDict = OrderedDict()
        self._failed_until = 0.0
        self._failure: Optional[BaseException] = None
        self._bench: OrderedDict = OrderedDict()
        self._bench_gates: dict = {}

    @property
    def demo(self) -> bool:
        return bool(getattr(self.source, "demo", False))

    def snapshot(self) -> Snapshot:
        with self._lock:
            t = self.clock()
            if self._failure is not None and t < self._failed_until:
                raise self._failure  # a recent read failed: no new psql until the backoff ends
            if self._last_refresh is None or t - self._last_refresh >= self.ttl:
                try:
                    self._refresh()
                except Exception as e:
                    self._failure = e
                    self._failed_until = self.clock() + FAIL_BACKOFF_S
                    raise
                self._failure = None
                self._last_refresh = self.clock()
            return self._snap

    def _refresh(self) -> None:
        src = self.source
        old = self._snap
        tasks, tr = src.tasks(TASK_LIMIT)
        events = old.events
        decisions = old.decisions
        trunc_ev = old.truncated_events
        trunc_dec = old.truncated_decisions
        if not self._loaded:
            events, trunc_ev = src.events_latest(EVENT_WINDOW)
            decisions = src.decisions_latest(DECISION_WINDOW)
            trunc_dec = len(decisions) >= DECISION_WINDOW  # the source cannot say; at the cap it may be
        else:
            events = list(events)
            decisions = list(decisions)
            for _ in range(MAX_PAGES_PER_REFRESH):
                last = events[-1]["id"] if events else 0
                new = src.events_after(last, PAGE)
                events.extend(e for e in new if e["id"] > last)
                if len(new) < PAGE:
                    break
            for _ in range(MAX_PAGES_PER_REFRESH):
                last = decisions[-1]["id"] if decisions else 0
                new = src.decisions_after(last, PAGE)
                decisions.extend(d for d in new if d["id"] > last)
                if len(new) < PAGE:
                    break
            if len(events) > 2 * EVENT_WINDOW:
                events = events[-EVENT_WINDOW:]
                trunc_ev = True
            if len(decisions) > 2 * DECISION_WINDOW:
                decisions = decisions[-DECISION_WINDOW:]
                trunc_dec = True
        self._loaded = True
        read_leases = getattr(src, "leases", None)
        leases, available = read_leases(TASK_LIMIT) if read_leases else ([], False)
        now = src.now()  # after the reads: the source learns which clock answered
        clock = getattr(src, "clock_source", "database")
        self._snap = Snapshot(old.version + 1, tasks, events, decisions, now, tr, trunc_ev, trunc_dec,
                              leases, available, clock)
        self._cache = OrderedDict()

    # ---------------------------------------------------------------- answers

    def meta(self, snap: Snapshot) -> dict:
        return {
            "demo": self.demo,
            "banner": "DEMO DATA, not your board" if self.demo else None,
            "now": snap.now,
            "clock": snap.clock,
            "leases_available": snap.leases_available,
            "stale_minutes": self.stale_s // 60,
            "open_hours": self.open_s // 3600,
            "truncated": {"tasks": snap.truncated_tasks, "events": snap.truncated_events,
                          "decisions": snap.truncated_decisions},
        }

    def now_payload(self) -> dict:
        s = self.snapshot()
        agents = model.build_agents(s.tasks, s.events, s.facts, s.now, self.stale_s)
        return {
            "meta": self.meta(s),
            "counts": model.counts_by_state(s.tasks),
            "total_tasks": len(s.tasks),
            "agents": agents,
            "attention": self._attention(s),
            "queue_totals": s.queue["totals"],
            "head": s.head,
        }

    def _attention(self, s: Snapshot) -> dict:
        a = model.attention(s.tasks, s.facts, s.now, self.stale_s, self.open_s)
        extra = model.queue_attention(s.queue)
        rules = dict(a["rules"])
        rules.update(extra.pop("queue_rules"))
        a.update(extra)
        a["rules"] = rules
        return a

    def queue_payload(self) -> dict:
        s = self.snapshot()
        q = dict(s.queue)
        q["meta"] = self.meta(s)
        q["head"] = s.head
        q["rules"] = {
            "runnable": "open, past its retry delay, every blocker done: what `board next` would take",
            "blocked": "open with a blocker that is not done yet",
            "claimed": "held under a lease; remaining time is lease_until minus the database clock",
            "retrying": "open again after a failure or an expired lease, waiting out its delay",
            "dead": "used all its attempts",
            "awaiting": "created with --needs-approval, waiting for `board approve`",
        }
        return q

    def task_payload(self, task_id: str) -> Optional[dict]:
        s = self.snapshot()
        view = model.task_view(task_id, s.tasks, s.events, s.decisions, s.now)
        if view is None:
            return None
        view["meta"] = self.meta(s)
        view["events_complete"] = not s.truncated_events and not view["events_truncated"]
        view["head"] = s.head
        return view

    def feed_payload(self, after, before, limit, agent, kind, task) -> dict:
        s = self.snapshot()
        reset = after is not None and after > s.head
        if after is not None and not reset:
            rows, cursor = model.feed_after(s.events, s.by_id, after, limit, agent, kind, task,
                                            model.FEED_BUDGET)
            has_more = cursor < s.head  # rows were left out; ask again from the cursor
        else:
            info: dict = {}
            rows = model.feed_page(s.events, s.by_id, None, before, limit, agent, kind, task,
                                   model.FEED_BUDGET, info)
            cursor = s.head
            has_more = len(rows) >= limit or info["cut"]
        return {
            "meta": self.meta(s),
            "events": rows,
            "cursor": cursor,
            "head": s.head,
            "reset": reset,
            "history_truncated": s.truncated_events,
            "agents": sorted({e["author"] for e in s.events}),
            "kinds": list(model.EVENT_KINDS),
            "has_more": has_more,
        }

    def scorecard_payload(self, win_a: tuple, win_b: Optional[tuple], agent: Optional[str]) -> dict:
        s = self.snapshot()
        win_a = _round_window(win_a)
        win_b = _round_window(win_b) if win_b else None
        key = (s.version, win_a, win_b, agent)
        if key in self._cache:
            self._cache.move_to_end(key)
            return self._cache[key]
        facts = s.facts
        te, td = s.truncated_events, s.truncated_decisions
        overall = metrics.compute_scorecard(facts, s.now, self.stale_s, win_a[0], win_a[1],
                                            truncated_events=te, truncated_decisions=td)
        claimants = sorted({f.claimant for f in facts if f.claimant})[:50]
        per_agent = {}
        for name in claimants:
            if agent and name != agent:
                continue
            per_agent[name] = metrics.compute_scorecard(facts, s.now, self.stale_s, win_a[0],
                                                        win_a[1], name, te, td)
        created = [t["created_at"] for t in s.tasks]
        out = {
            "meta": self.meta(s),
            "overall": overall,
            "agents": per_agent,
            "agent_names": claimants,
            "not_measurable": metrics.NOT_MEASURABLE,
            "task_time_range": {"min": min(created) if created else None,
                                "max": max(created) if created else None},
            "compare": (metrics.compare_windows(facts, win_a, win_b, agent, te) if win_b else None),
            "note": "Computed from the %d tasks and %d events loaded; every metric shows n and a "
                    "'not enough data' label under its minimum." % (len(s.tasks), len(s.events)),
        }
        self._cache[key] = out
        while len(self._cache) > SCORECARD_CACHE:
            self._cache.popitem(last=False)
        return out

    # ---------------------------------------------------------------- experiments

    def _bench_meta(self) -> dict:
        """The meta block for the experiment screen, which needs the clock and the demo flag but not the
        board's tasks, events or decisions."""
        return {"demo": self.demo, "banner": "DEMO DATA, not your board" if self.demo else None,
                "now": self.source.now(), "clock": getattr(self.source, "clock_source", "database"),
                "bench_available": bench.available()}

    def _bench_cached(self, key, make):
        """The answer for KEY, recomputed at most once per BENCH_TTL_S however many requests arrive together: a
        lock per key makes concurrent requests wait for the first one's bootstrap instead of repeating it."""
        with self._lock:
            hit = self._bench.get(key)
            if hit is not None and self.clock() - hit[0] < BENCH_TTL_S:
                self._bench.move_to_end(key)
                return hit[1]
            gate = self._bench_gates.setdefault(key, threading.Lock())
        with gate:
            with self._lock:
                hit = self._bench.get(key)
                if hit is not None and self.clock() - hit[0] < BENCH_TTL_S:
                    return hit[1]
            value = make()  # outside the service lock: it may run psql
            with self._lock:
                self._bench[key] = (self.clock(), value)
                self._bench.move_to_end(key)
                while len(self._bench) > BENCH_CACHE:
                    old_key, _ = self._bench.popitem(last=False)
                    self._bench_gates.pop(old_key, None)
            return value

    def bench_list_payload(self) -> dict:
        return self._bench_cached(("list",), lambda: bench.list_payload(self.source, self._bench_meta()))

    def bench_payload(self, name: str, rule: str = "majority") -> Optional[dict]:
        out = self._bench_cached(("exp", name, rule),
                                 lambda: bench.experiment_payload(self.source, name, self._bench_meta(), rule))
        return out
