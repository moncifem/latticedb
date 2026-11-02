"""An executable model of the board's lease, fence, retry and dependency rules.

This is the specification the shell helper (integrations/board/board) implements, written as
ordinary Python over a fake in-memory database that behaves like the Lattice alpha where it
matters here:

  * snapshot isolation: a transaction reads the state it began with;
  * first committer wins, per row: if another transaction committed a write to a row this one
    also wrote, commit fails with Conflict (SQLSTATE 40001, reported at COMMIT);
  * every board write bumps the shared 'events' counter row, so those writers conflict with each
    other, while a heartbeat touches only its lease row;
  * the database clock (`Board.clock`) is the only clock a lease or a backoff is compared with.

Each operation is a generator: it yields wherever another agent may run (between its reads and
its commit, between two transactions). Its return value is what the helper would report. The
checks that matter are made inside Board.commit, on the committed state, not by the operations
themselves, so a bug in an operation cannot hide from them.

This model is NOT the SQL. It shows that the rules, as written, hold under interleavings and
that the checks can fail (BUGS plants one fault each). Whether the shell script's SQL matches
the rules is tested by test.sh against latticed and, without a database, by the fake-psql tests.
"""

from __future__ import annotations

import random
from collections import Counter

BUGS = (
    "stale_fence_accepted",   # done/note/fail/release check the owner but not the fence
    "heartbeat_ignores_fence",  # heartbeat checks the owner but not the fence
    "reap_without_cas",       # reap writes from an earlier look, with no compare in the write
    "done_twice_appends",     # a second done writes a second event
    "claim_ignores_blockers",  # next claims a task whose blockers are not done
    "fail_never_dead",        # fail always reopens, so attempts pass the limit
)

LEASE_DEFAULT = {"lease_until": 0, "heartbeat_at": 0, "attempts": 0, "max_attempts": 3,
                 "not_before": 0, "last_error": "", "blocked_by": ()}
FENCED = ("done", "note", "fail", "release", "heartbeat")


class Conflict(Exception):
    """Serialization failure (40001) reported at COMMIT."""


def backoff_for(attempts: int) -> int:
    """30 doubled once per earlier failure, at most 3600 (the helper's backoff_for)."""
    s = 30
    for _ in range(attempts):
        if s >= 3600:
            break
        s *= 2
    return min(s, 3600)


class Txn:
    def __init__(self, board, label, agent, fence=None):
        self.b = board
        self.label = label
        self.agent = agent
        self.fence = fence
        self.start = board.commit_no
        self.tasks = {k: dict(v) for k, v in board.tasks.items()}
        self.leases = {k: dict(v) for k, v in board.leases.items()}
        self.counter = board.counter
        self.writes: set = set()
        self.new_events: list = []
        self.new_tasks: list = []
        self.mutated = False  # set by operations that changed task or lease state on purpose

    # -- statements
    def bump(self) -> int:
        self.counter += 1
        self.writes.add(("counters", "events"))
        return self.counter

    def task(self, tid):
        return self.tasks[tid]

    def lease(self, tid):
        return self.leases.get(tid) or dict(LEASE_DEFAULT)

    def set_task(self, tid, **kw):
        self.tasks[tid].update(kw)
        self.writes.add(("tasks", tid))

    def set_lease(self, tid, **kw):
        row = self.leases.setdefault(tid, dict(LEASE_DEFAULT))
        row.update(kw)
        self.writes.add(("leases", tid))

    def add_event(self, seq, tid, author, kind, body=""):
        self.new_events.append({"id": seq, "task_id": tid, "author": author, "kind": kind,
                                "body": body, "created_at": self.b.clock})
        self.writes.add(("events", seq))

    def add_task(self, tid, **fields):
        row = {"id": tid, "state": "open", "owner": None, "priority": 0, "claim_seq": 0,
               "done_seq": 0, "created_seq": 0}
        row.update(fields)
        self.tasks[tid] = row
        self.new_tasks.append(tid)
        self.writes.add(("tasks", tid))

    def commit(self):
        for key in self.writes:
            if self.b.version.get(key, 0) > self.start:
                self.b.stats["conflict"] += 1
                raise Conflict()
        self.b._apply(self)


class Board:
    def __init__(self, bugs=(), clock=1_000_000):
        unknown = set(bugs) - set(BUGS)
        if unknown:
            raise ValueError("unknown bug %s" % sorted(unknown))
        self.bugs = set(bugs)
        self.clock = clock
        self.tasks: dict = {}
        self.leases: dict = {}
        self.events: list = []
        self.counter = 0
        self.commit_no = 0
        self.version: dict = {}
        self.violations: list = []
        self.stats: Counter = Counter()

    def begin(self, label, agent, fence=None) -> Txn:
        return Txn(self, label, agent, fence)

    def violation(self, kind, detail):
        self.violations.append((kind, detail))

    # -- what a commit checks. These are the invariants.
    def _apply(self, t: Txn) -> None:
        now = self.clock
        for key in sorted(t.writes):
            table, k = key
            if table == "tasks":
                old = self.tasks.get(k)
                new = t.tasks[k]
                self._check_task(t, old, new, now)
            elif table == "leases":
                new = t.leases[k]
                if new["attempts"] > new["max_attempts"]:
                    self.violation("attempts_exceed_max", k)
                if t.label == "heartbeat" and t.mutated:
                    # a heartbeat writes only the lease row, so the fence is checked here
                    cur = self.tasks[k]
                    if not (cur["state"] == "claimed" and cur["owner"] == t.agent
                            and cur["claim_seq"] == t.fence):
                        self.violation("stale_write_accepted", (t.label, k, t.agent, t.fence))
        for ev in t.new_events:
            if ev["kind"] == "done" and any(e["task_id"] == ev["task_id"] and e["kind"] == "done"
                                            for e in self.events):
                self.violation("done_event_twice", ev["task_id"])
        # apply
        self.commit_no += 1
        for key in t.writes:
            table, k = key
            if table == "tasks":
                self.tasks[k] = t.tasks[k]
            elif table == "leases":
                self.leases[k] = t.leases[k]
            self.version[key] = self.commit_no
        self.counter = max(self.counter, t.counter)
        self.events.extend(t.new_events)
        for k in t.writes:
            if k[0] == "tasks" and self.tasks[k[1]]["state"] == "open":
                lease = self.leases.get(k[1], LEASE_DEFAULT)
                if lease["attempts"] >= lease["max_attempts"]:
                    self.violation("open_with_no_attempts_left", k[1])

    def _check_task(self, t, old, new, now):
        tid = new["id"]
        if old is None:
            return
        if old["state"] == "done" and new["state"] != "done":
            self.violation("done_reopened", tid)
        if old["state"] == "claimed" and new["state"] == "claimed" and (
                old["owner"] != new["owner"] or old["claim_seq"] != new["claim_seq"]):
            self.violation("lease_stolen", tid)
        if new["state"] == "claimed" and old["state"] != "claimed":
            if old["state"] != "open" or old["owner"] is not None:
                self.violation("claim_from_bad_state", (tid, old["state"]))
            lease = self.leases.get(tid, LEASE_DEFAULT)
            if lease["not_before"] > now:
                self.violation("claimed_before_not_before", tid)
            for b in lease["blocked_by"]:
                if b not in self.tasks or self.tasks[b]["state"] != "done":
                    self.violation("claimed_while_blocked", (tid, b))
        if t.label == "reap" and old["state"] == "claimed" and new["state"] != "claimed":
            lease = self.leases.get(tid, LEASE_DEFAULT)
            if not (0 < lease["lease_until"] < now):
                self.violation("reaped_live_lease", tid)
        if t.label in FENCED and t.mutated and t.fence is not None:
            held = (old["state"] == "claimed" and old["owner"] == t.agent
                    and old["claim_seq"] == t.fence)
            if not held:
                self.violation("stale_write_accepted", (t.label, tid, t.agent, t.fence))


# ------------------------------------------------------------------------------ operations
# Each generator returns what the helper would print or exit with. `p` is a Proc.

MAX_TX_TRIES = 25


class Proc:
    """One running helper process. Two procs may share a name (a zombie and its replacement)."""

    def __init__(self, name, lease=60, cap=1):
        self.name = name
        self.lease = lease
        self.cap = cap
        self.held: dict = {}  # task id -> fence this process believes it has


def op_add(b: Board, p: Proc, tid, priority=0, blocked_by=(), needs_approval=False, max_attempts=3):
    while True:
        t = b.begin("add", p.name)
        seq = t.bump()
        t.add_task(tid, state="awaiting_approval" if needs_approval else "open", priority=priority,
                   created_seq=seq)
        t.set_lease(tid, max_attempts=max_attempts, blocked_by=tuple(blocked_by))
        t.add_event(seq, tid, p.name, "add")
        yield
        try:
            t.commit()
            return ("added", tid)
        except Conflict:
            yield


def op_claim(b: Board, p: Proc, tid):
    """cmd_claim/do_claim: take tid if it is open, not backing off and unblocked."""
    lease0 = b.leases.get(tid, LEASE_DEFAULT)
    if "claim_ignores_blockers" not in b.bugs:
        pending = [x for x in lease0["blocked_by"] if x not in b.tasks or b.tasks[x]["state"] != "done"]
        if pending:
            return ("blocked", pending)
    yield
    for _ in range(MAX_TX_TRIES):
        t = b.begin("claim", p.name)
        seq = t.bump()
        now = b.clock
        ready = t.lease(tid)["not_before"] <= now
        if ready:
            tk = t.task(tid)
            if tk["owner"] is None and tk["state"] == "open":
                t.set_task(tid, owner=p.name, state="claimed", claim_seq=seq)
                t.add_event(seq, tid, p.name, "claim")
            if t.task(tid)["claim_seq"] == seq:
                t.set_lease(tid, lease_until=now + p.lease, heartbeat_at=now)
        tk = t.task(tid)
        row = (tk["claim_seq"] == seq, tk["owner"], tk["state"], tk["claim_seq"], ready)
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        won, owner, state, cseq, ready = row
        if won:
            b.stats["claim_won"] += 1
            p.held[tid] = cseq
            return ("won", cseq)
        if owner == p.name and state == "claimed":
            p.held[tid] = cseq
            return ("yours", cseq)
        return ("backoff" if not ready else "lost",)
    return ("busy",)


def op_next(b: Board, p: Proc):
    mine = [x for x in b.tasks.values() if x["state"] == "claimed" and x["owner"] == p.name]
    if len(mine) >= p.cap:
        return ("full",)
    yield from op_reap_all(b, p)
    now = b.clock
    cands = sorted((x for x in b.tasks.values() if x["state"] == "open" and x["owner"] is None),
                   key=lambda x: (-x["priority"], x["created_seq"]))[:50]
    yield
    for c in cands:
        lease = b.leases.get(c["id"], LEASE_DEFAULT)
        if lease["not_before"] > now:
            continue
        res = yield from op_claim(b, p, c["id"])
        if res[0] in ("won", "yours"):
            return ("claimed", c["id"], res[1])
    return ("none",)


def _held(tk, p, fence, bugs, ignore_bug):
    if tk["state"] != "claimed" or tk["owner"] != p.name:
        return False
    return True if ignore_bug in bugs else tk["claim_seq"] == fence


def op_heartbeat(b: Board, p: Proc, tid):
    fence = p.held.get(tid)
    while True:
        t = b.begin("heartbeat", p.name, fence)
        now = b.clock
        held = _held(t.task(tid), p, fence, b.bugs, "heartbeat_ignores_fence")
        if held:
            t.set_lease(tid, lease_until=now + p.lease, heartbeat_at=now)
            t.mutated = True
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        if held:
            b.stats["heartbeat_ok"] += 1
            return ("ok",)
        b.stats["heartbeat_lost"] += 1
        p.held.pop(tid, None)
        return ("lost",)


def op_note(b: Board, p: Proc, tid):
    fence = p.held.get(tid)
    while True:
        t = b.begin("note", p.name, fence)
        seq = t.bump()
        held = _held(t.task(tid), p, fence, b.bugs, "stale_fence_accepted")
        if held:
            t.set_task(tid, updated_at=b.clock)
            t.add_event(seq, tid, p.name, "note")
            t.mutated = True
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        if held:
            return ("noted",)
        b.stats["refused"] += 1
        p.held.pop(tid, None)
        return ("refused",)


def op_done(b: Board, p: Proc, tid):
    fence = p.held.get(tid)
    if fence is None:
        return ("nofence",)
    if b.tasks[tid]["state"] == "done" and "done_twice_appends" not in b.bugs:
        yield
        p.held.pop(tid, None)
        b.stats["done_noop"] += 1
        return ("noop",)
    yield
    while True:
        t = b.begin("done", p.name, fence)
        seq = t.bump()
        tk = t.task(tid)
        ok = _held(tk, p, fence, b.bugs, "stale_fence_accepted")
        if "done_twice_appends" in b.bugs and tk["state"] == "done" and tk["owner"] == p.name:
            ok = True
        if ok:
            t.set_task(tid, state="done", done_seq=seq)
            t.add_event(seq, tid, p.name, "done")
            t.set_lease(tid, lease_until=0)
            t.mutated = True
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        if ok:
            b.stats["done"] += 1
            return ("done",)
        state = b.tasks[tid]["state"]
        p.held.pop(tid, None)
        if state == "done":
            b.stats["done_noop"] += 1
            return ("noop",)
        if state in ("dead", "cancelled", "awaiting_approval"):
            return ("error", state)
        b.stats["refused"] += 1
        return ("refused",)


def op_fail(b: Board, p: Proc, tid, retry_in=None):
    fence = p.held.get(tid)
    if fence is None:
        return ("nofence",)
    attempts = b.leases.get(tid, LEASE_DEFAULT)["attempts"]
    delay = retry_in if retry_in is not None else backoff_for(attempts)
    yield
    while True:
        t = b.begin("fail", p.name, fence)
        seq = t.bump()
        now = b.clock
        ok = _held(t.task(tid), p, fence, b.bugs, "stale_fence_accepted")
        exhausted = False
        if ok:
            lease = t.lease(tid)
            exhausted = lease["attempts"] + 1 >= lease["max_attempts"]
            if "fail_never_dead" in b.bugs:
                exhausted = False
            if exhausted:
                t.set_task(tid, state="dead", owner=None)
                t.set_lease(tid, attempts=lease["attempts"] + 1, lease_until=0, last_error="failed")
            else:
                t.set_task(tid, state="open", owner=None)
                t.set_lease(tid, attempts=lease["attempts"] + 1, lease_until=0,
                            not_before=now + delay, last_error="failed")
            t.add_event(seq, tid, p.name, "fail")
            t.mutated = True
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        p.held.pop(tid, None)
        if not ok:
            b.stats["refused"] += 1
            return ("refused",)
        b.stats["fail_dead" if exhausted else "fail_retry"] += 1
        return ("dead" if exhausted else "retry",)


def op_release(b: Board, p: Proc, tid):
    fence = p.held.get(tid)
    if fence is None:
        return ("nofence",)
    while True:
        t = b.begin("release", p.name, fence)
        seq = t.bump()
        ok = _held(t.task(tid), p, fence, b.bugs, "stale_fence_accepted")
        if ok:
            t.set_task(tid, state="open", owner=None)
            t.set_lease(tid, lease_until=0)
            t.add_event(seq, tid, p.name, "release")
            t.mutated = True
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        p.held.pop(tid, None)
        if ok:
            b.stats["release"] += 1
            return ("released",)
        b.stats["refused"] += 1
        return ("refused",)


def op_reap_one(b: Board, p: Proc, tid):
    info = b.tasks[tid]
    if info["state"] != "claimed":
        return False
    fence = info["claim_seq"]
    attempts = b.leases.get(tid, LEASE_DEFAULT)["attempts"]
    delay = backoff_for(attempts)
    yield
    while True:
        t = b.begin("reap", p.name)
        seq = t.bump()
        now = b.clock
        tk = t.task(tid)
        lease = t.lease(tid)
        if "reap_without_cas" in b.bugs:
            ok = True  # the candidate was chosen from an earlier look; nothing is compared again
        else:
            ok = tk["state"] == "claimed" and tk["claim_seq"] == fence \
                and 0 < lease["lease_until"] < now
        exhausted = False
        if ok:
            exhausted = lease["attempts"] + 1 >= lease["max_attempts"]
            if exhausted:
                t.set_task(tid, state="dead", owner=None)
                t.set_lease(tid, attempts=lease["attempts"] + 1, lease_until=0, last_error="lease expired")
            else:
                t.set_task(tid, state="open", owner=None)
                t.set_lease(tid, attempts=lease["attempts"] + 1, lease_until=0,
                            not_before=now + delay, last_error="lease expired")
            t.add_event(seq, tid, p.name, "expired")
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        if ok:
            b.stats["expired_dead" if exhausted else "expired_retry"] += 1
        return ok


def op_reap_all(b: Board, p: Proc):
    now = b.clock
    cands = sorted((tid for tid, lease in b.leases.items()
                    if 0 < lease["lease_until"] < now and b.tasks[tid]["state"] == "claimed"),
                   key=lambda x: b.leases[x]["lease_until"])[:100]
    n = 0
    for tid in cands:
        yield
        got = yield from op_reap_one(b, p, tid)
        n += 1 if got else 0
    return n


def op_approve(b: Board, p: Proc, tid):
    while True:
        t = b.begin("approve", p.name)
        seq = t.bump()
        ok = t.task(tid)["state"] == "awaiting_approval"
        if ok:
            t.set_task(tid, state="open")
            t.add_event(seq, tid, p.name, "approve")
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        return ok


def op_cancel(b: Board, p: Proc, tid):
    while True:
        t = b.begin("cancel", p.name)
        seq = t.bump()
        ok = t.task(tid)["state"] in ("open", "claimed", "awaiting_approval")
        if ok:
            t.set_task(tid, state="cancelled", owner=None)
            t.set_lease(tid, lease_until=0)
            t.add_event(seq, tid, p.name, "cancel")
        yield
        try:
            t.commit()
        except Conflict:
            yield
            continue
        return ok


# ------------------------------------------------------------------------------ a whole run

class Run:
    """Random agents against one Board. Deterministic for a seed."""

    def __init__(self, seed, bugs=(), steps=700, lease=40):
        self.rng = random.Random(seed)
        self.b = Board(bugs)
        self.steps = steps
        self.lease = lease
        names = [("a", 1), ("b", 2), ("b", 2), ("c", 2), ("d", 1)]  # b has a twin process
        self.procs = [Proc(n, lease=lease, cap=cap) for n, cap in names]
        self.maint = Proc("maint")
        self.reaper = Proc("reaper")
        self.inflight: dict = {}  # id(proc) -> generator
        self.paused: dict = {}  # id(proc) -> steps left
        self.added: list = []
        self.next_task = 1

    def _start(self, p):
        rng, b = self.rng, self.b
        if p is self.maint:
            r = rng.random()
            if r < 0.55 or not self.added:
                pool = self.added[-6:]
                blockers = rng.sample(pool, k=min(len(pool), rng.choice([0, 0, 0, 1, 2])))
                tid = self._take()
                return op_add(b, p, tid, priority=rng.randint(0, 3), blocked_by=blockers,
                              needs_approval=rng.random() < 0.12, max_attempts=rng.choice([2, 3, 3]))
            tid = rng.choice(self.added)
            st = b.tasks.get(tid, {}).get("state")
            if st == "awaiting_approval":
                return op_approve(b, p, tid)
            if rng.random() < 0.15 and st in ("open", "claimed"):
                return op_cancel(b, p, tid)
            return op_add(b, p, self._take(), priority=0)
        if p is self.reaper:
            return op_reap_all(b, p)
        if not p.held:
            return op_next(b, p)
        tid = rng.choice(sorted(p.held))
        r = rng.random()
        if r < 0.40:
            return op_heartbeat(b, p, tid)
        if r < 0.55:
            return op_note(b, p, tid)
        if r < 0.78:
            return op_done(b, p, tid)
        if r < 0.88:
            return op_fail(b, p, tid, retry_in=rng.choice([None, 0, 5]))
        if r < 0.93:
            return op_release(b, p, tid)
        # replay of a finished call: done again with the fence the process remembers
        return op_done(b, p, tid)

    def _take(self):
        tid = "t%d" % self.next_task
        self.next_task += 1
        self.added.append(tid)
        return tid

    def _advance_clock(self):
        r = self.rng.random()
        if r < 0.70:
            self.b.clock += self.rng.randint(0, 3)
        elif r < 0.95:
            self.b.clock += self.rng.randint(5, 30)
        else:
            self.b.clock += self.rng.randint(60, 200)

    def _step(self, p):
        key = id(p)
        if self.paused.get(key, 0) > 0:
            self.paused[key] -= 1
            return
        gen = self.inflight.get(key)
        if gen is None:
            gen = self._start(p)
            self.inflight[key] = gen
        try:
            next(gen)
        except StopIteration:
            del self.inflight[key]
            if p not in (self.maint, self.reaper) and self.rng.random() < 0.10:
                self.paused[key] = self.rng.randint(5, 60)  # a stall: the lease may run out
        # a process may also be killed mid-call: it forgets the call but keeps what it believed

    def run(self):
        actors = self.procs + [self.maint, self.reaper, self.reaper]
        for _ in range(self.steps):
            self._advance_clock()
            self._step(self.rng.choice(actors))
        self.finish()
        return self.b

    def finish(self):
        """Let every call end, then let every lease run out and reap, then check the end state."""
        b = self.b
        self.paused.clear()
        for _ in range(400):
            if not self.inflight:
                break
            for p in self.procs + [self.maint, self.reaper]:
                gen = self.inflight.get(id(p))
                if gen is not None:
                    try:
                        next(gen)
                    except StopIteration:
                        del self.inflight[id(p)]
        b.clock += 10_000
        for _ in range(5):
            gen = op_reap_all(b, self.reaper)
            for _ in gen:
                pass
            b.clock += 10_000
        b_tasks = b.tasks
        for tid in self.added:
            if tid not in b_tasks:
                b.violation("task_lost", tid)
        for tid, tk in b_tasks.items():
            if tk["state"] not in ("open", "done", "dead", "cancelled", "awaiting_approval"):
                b.violation("task_not_settled", (tid, tk["state"]))
        for tid in b_tasks:
            n = sum(1 for e in b.events if e["task_id"] == tid and e["kind"] == "done")
            if n > 1:
                b.violation("done_event_twice", tid)


def run_seed(seed, bugs=(), steps=700):
    return Run(seed, bugs, steps).run()


def first_violation(bug, seeds=range(1, 400), steps=700):
    """The first (seed, violation kinds) that a planted bug produces, or None."""
    for s in seeds:
        b = run_seed(s, (bug,), steps)
        if b.violations:
            return s, sorted({v[0] for v in b.violations})
    return None
