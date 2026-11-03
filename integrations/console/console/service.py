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

from . import activity as act
from . import bench, metrics, model
from .source import SourceError, scrub

TASK_LIMIT = 5000
EVENT_WINDOW = 20000
DECISION_WINDOW = 5000
PAGE = 2000
MAX_PAGES_PER_REFRESH = 10
FAIL_BACKOFF_S = 2.0  # after a failed read, answer from the failure instead of starting psql again
SCORECARD_CACHE = 16
BENCH_CACHE = 8
BENCH_TTL_S = 5.0  # an experiment report costs a bootstrap per pair of arms; reuse it for a few seconds
ACTIVITY_SESSIONS = 500  # session rows read per refresh (newest by last_seen)
ACTIVITY_WINDOW = 5000  # log rows read the first time; the board keeps a ring of 200 per agent
ACTIVITY_PAGE = 2000
ACTIVITY_RETRY_S = 30.0  # after "no activity tables" or a refusal, ask again this often
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
    activity_sessions: list = field(default_factory=list)
    activity_log: list = field(default_factory=list)
    activity_available: bool = False
    activity_note: str = ""
    quiet_s: int = 600
    _activity: Optional[dict] = None
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

    @property
    def activity_head(self) -> int:
        return self.activity_log[-1]["id"] if self.activity_log else 0

    @property
    def activity(self) -> dict:
        """{agent: activity block or None} for every agent the board knows."""
        if self._activity is None:
            names = sorted({e["author"] for e in self.events} | {t["owner"] for t in self.tasks if t.get("owner")}
                           | {x["agent"] for x in self.activity_sessions})
            self._activity = act.activity_for_agents(names, self.activity_sessions, self.activity_log, self.tasks,
                                                     self.queue, self.now, self.quiet_s)
        return self._activity


def _round_window(w: tuple) -> tuple:
    r = WINDOW_ROUND_S
    return tuple(None if x is None else x // r * r for x in w)


class BoardService:
    def __init__(self, source, stale_minutes: int = 30, open_hours: int = 24, ttl: float = 1.0,
                 clock=time.monotonic, quiet_minutes: int = 10):
        if not 1 <= stale_minutes <= 60 * 24 * 30:
            raise ValueError("stale_minutes must be between 1 and 43200")
        if not 1 <= open_hours <= 24 * 365:
            raise ValueError("open_hours must be between 1 and 8760")
        if not 1 <= quiet_minutes <= 60 * 24:
            raise ValueError("quiet_minutes must be between 1 and 1440")
        self.quiet_s = quiet_minutes * 60
        self._activity_retry_at = 0.0
        self._activity_cursor = 0  # the newest log id read; rows dropped from the copy must not be read again
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
        a_sessions, a_log, a_ok, a_note = self._refresh_activity(old)
        self._snap = Snapshot(old.version + 1, tasks, events, decisions, now, tr, trunc_ev, trunc_dec,
                              leases, available, clock, a_sessions, a_log, a_ok, a_note, self.quiet_s)
        self._cache = OrderedDict()

    def _refresh_activity(self, old: Snapshot) -> tuple:
        """(sessions, log rows, available, note). Never raises: a board without the activity tables, or an
        account that may not read them, still gets every other screen."""
        src = self.source
        if not hasattr(src, "activity_sessions"):
            return [], [], False, ""
        if not old.activity_available and old.version and self.clock() < self._activity_retry_at:
            return [], [], False, old.activity_note
        try:
            raw, available = src.activity_sessions(ACTIVITY_SESSIONS)
            if not available:
                self._activity_retry_at = self.clock() + ACTIVITY_RETRY_S
                return [], [], False, "The activity tables are not there or this account may not read them (board init creates them)."
            sessions = [r for r in (act.clean_session(x) for x in raw) if r is not None]
            log_rows = list(old.activity_log) if old.activity_available else []
            judged_up_to = self._activity_cursor  # rows above it arrive in this refresh and are judged in the next
            if not log_rows:
                log_rows = [r for r in (act.clean_log(x) for x in src.activity_latest(ACTIVITY_WINDOW)) if r]
                log_rows.sort(key=lambda r: r["id"])
                self._activity_cursor = log_rows[-1]["id"] if log_rows else 0
            else:
                for _ in range(MAX_PAGES_PER_REFRESH):
                    new = src.activity_after(self._activity_cursor, ACTIVITY_PAGE)
                    fresh = sorted((r for r in (act.clean_log(x) for x in new) if r), key=lambda r: r["id"])
                    log_rows = act.merge_log(log_rows, fresh)
                    if fresh:
                        self._activity_cursor = max(self._activity_cursor, fresh[-1]["id"])
                    if len(new) < ACTIVITY_PAGE:
                        break
            # Rows the board no longer holds (board activity-purge) must not stay in this copy.
            # A session can start between the two reads (or write its log row before its session row exists), so a row
            # that arrived just now may have no session row yet: only the rows held before this refresh are judged.
            held = [r for r in log_rows if r["id"] <= judged_up_to]
            fresh_rows = [r for r in log_rows if r["id"] > judged_up_to]
            log_rows = act.drop_purged(held, sessions, len(raw) >= ACTIVITY_SESSIONS) + fresh_rows
            return sessions, log_rows, True, ""
        except SourceError as e:
            self._activity_retry_at = self.clock() + ACTIVITY_RETRY_S
            # Keep what was read before: a hiccup must not blank the screens.
            return (old.activity_sessions, old.activity_log, old.activity_available,
                    "Activity could not be read: " + scrub(str(e))[:200])

    # ---------------------------------------------------------------- answers

    def meta(self, snap: Snapshot) -> dict:
        return {
            "demo": self.demo,
            "banner": "DEMO DATA, not your board" if self.demo else None,
            "now": snap.now,
            "clock": snap.clock,
            "leases_available": snap.leases_available,
            "activity_available": snap.activity_available,
            "stale_minutes": self.stale_s // 60,
            "open_hours": self.open_s // 3600,
            "truncated": {"tasks": snap.truncated_tasks, "events": snap.truncated_events,
                          "decisions": snap.truncated_decisions},
        }

    def now_payload(self) -> dict:
        s = self.snapshot()
        agents = model.build_agents(s.tasks, s.events, s.facts, s.now, self.stale_s)
        for a in agents:
            a["activity"] = s.activity.get(a["name"]) if s.activity_available else None
        if s.activity_available:
            # An agent that reports activity but never touched the board (a plain session with a BOARD_AUTHOR)
            # is still an agent worth seeing.
            known = {a["name"] for a in agents}
            for name in sorted({x["agent"] for x in s.activity_sessions} - known):
                agents.append({"name": name, "state": "idle", "current_task": None, "claimed": [], "last_event": None,
                               "events": 0, "tasks_done": 0, "activity": s.activity.get(name)})
            order = {"stale": 0, "working": 1, "idle": 2}
            agents.sort(key=lambda a: (order[a["state"]], a["name"]))
        return {
            "meta": self.meta(s),
            "counts": model.counts_by_state(s.tasks),
            "total_tasks": len(s.tasks),
            "agents": agents,
            "attention": self._attention(s),
            "queue_totals": s.queue["totals"],
            "head": s.head,
            "activity": {"available": s.activity_available, "head": s.activity_head, "note": s.activity_note,
                         "working_s": act.WORKING_S, "quiet_minutes": self.quiet_s // 60,
                         "strip_minutes": act.STRIP_MINUTES,
                         "states": act.STATE_TEXT},
        }

    def _attention(self, s: Snapshot) -> dict:
        a = model.attention(s.tasks, s.facts, s.now, self.stale_s, self.open_s)
        extra = model.queue_attention(s.queue)
        rules = dict(a["rules"])
        rules.update(extra.pop("queue_rules"))
        a.update(extra)
        a["quiet_claims"] = (act.quiet_claims(s.activity, s.by_id, s.queue, s.now) if s.activity_available else [])
        rules["quiet_claims"] = act.QUIET_RULE % (self.quiet_s // 60)
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

    def agent_payload(self, name: str, tool, outcome, task, limit: int, after) -> Optional[dict]:
        """The agent screen: its sessions, its timeline (newest first, or the rows after a cursor), the tool
        mix, the active time estimate. None when nothing in the loaded data knows the agent."""
        s = self.snapshot()
        known = ({e["author"] for e in s.events} | {t["owner"] for t in s.tasks if t.get("owner")}
                 | {x["agent"] for x in s.activity_sessions} | {x["agent"] for x in s.activity_log})
        if name not in known:
            return None
        d = act.agent_detail(name, s.activity_sessions, s.activity_log, s.tasks, s.now, tool, outcome, task,
                             limit, after)
        d["meta"] = self.meta(s)
        d["available"] = s.activity_available
        d["note"] = s.activity_note
        d["activity"] = s.activity.get(name) if s.activity_available else None
        d["head"] = s.activity_head
        return d

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
            "activity": (act.active_per_task(facts, s.activity_log) if s.activity_available else None),
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
