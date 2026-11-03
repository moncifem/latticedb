"""Live agent activity: what each agent is doing now, from the rows `board activity` writes.

Pure functions, like model.py: `now` is passed in and is the database clock. The rows are written by the
agent's own hook (integrations/board/hooks/activity-hook.py), so everything here treats them as untrusted,
self-reported text: the cleaners below enforce the shape the helper promises even when a row was written by
hand, and the wording never claims more than the rows show ("no tool activity for 12 min", not "stuck").
"""

from __future__ import annotations

import re
from typing import Optional

WORKING_S = 120  # activity at most this many seconds old: working
STRIP_MINUTES = 60
STRIP_BUCKET_S = 60
GAP_S = 60  # consecutive rows closer than this count as one stretch of work (the active time estimate)
DETAIL_LIMIT = 200
DETAIL_SESSIONS = 20
MIN_TASKS = 5  # fewer measured tasks than this: no number is shown
LOG_KEEP = 20000  # rows the console keeps in memory

TOOL_RX = re.compile(r"[A-Za-z0-9_.-]{1,64}")
AGENT_RX = re.compile(r"[A-Za-z0-9._-]{1,64}")
TASK_RX = re.compile(r"[A-Za-z0-9_-]{1,64}")
MIX_ITEM_RX = re.compile(r"([A-Za-z0-9_.-]{1,64})=([0-9]{1,9})")
OUTCOMES = ("ok", "error", "denied")
SESSION_STATES = ("working", "idle", "ended")
# Rows that mark a session start, end or the end of a turn: they carry no tool call.
LIFECYCLE_TOOLS = ("session-start", "session-end", "stop", "prompt")
# control characters, line and paragraph separators, and the invisible ones that reorder or hide text:
# zero-width characters and the left-to-right and right-to-left marks (U+200B-U+200F), the embedding and
# override controls (U+202A-U+202E), the isolates (U+2066-U+2069), the Arabic letter mark (U+061C), the word joiner
# (U+2060) and the byte order mark (U+FEFF)
_CONTROL = re.compile("[\x00-\x1f\x7f-\x9f\u061c\u200b-\u200f\u2028\u2029\u202a-\u202e\u2060\u2066-\u2069\ufeff]")

STATE_TEXT = {
    "working": "tool activity in the last %d s" % WORKING_S,
    "quiet": "holds a task, no tool activity for a while",
    "stuck?": "holds a task, its lease is alive, tool activity stopped",
    "idle": "holds no task and has no recent activity",
}


def clip_text(value, limit: int = 64) -> str:
    s = "" if value is None else str(value)
    return _CONTROL.sub("", s)[:limit]


def clean_session(row: dict) -> Optional[dict]:
    """The session row in the shape the helper writes, or None when it is not that shape."""
    agent, session = row.get("agent"), row.get("session")
    if not (isinstance(agent, str) and AGENT_RX.fullmatch(agent) and isinstance(session, str)
            and AGENT_RX.fullmatch(session)):
        return None
    tool = row.get("tool")
    task = row.get("task_id") or ""
    state = row.get("state")
    outcome = row.get("outcome")
    out = {
        "id": clip_text(row.get("id"), 130),
        "agent": agent, "session": session,
        "task_id": task if isinstance(task, str) and (task == "" or TASK_RX.fullmatch(task)) else "",
        "tool": tool if isinstance(tool, str) and TOOL_RX.fullmatch(tool) else "other",
        "target": clip_text(row.get("target"), 64),
        "outcome": outcome if outcome in OUTCOMES else "ok",
        "state": state if state in SESSION_STATES else "working",
    }
    for key in ("tool_calls", "last_seen", "started_at"):
        v = row.get(key)
        out[key] = v if isinstance(v, int) and not isinstance(v, bool) and v >= 0 else 0
    return out


def clean_log(row: dict) -> Optional[dict]:
    base = clean_session(dict(row, state="working"))
    if base is None:
        return None
    rid, calls, at = row.get("id"), row.get("calls"), row.get("at")
    if not (isinstance(rid, int) and not isinstance(rid, bool) and rid >= 0
            and isinstance(at, int) and not isinstance(at, bool) and at >= 0):
        return None
    return {"id": rid, "agent": base["agent"], "session": base["session"], "task_id": base["task_id"],
            "tool": base["tool"], "target": base["target"], "outcome": base["outcome"],
            "calls": calls if isinstance(calls, int) and not isinstance(calls, bool) and 0 <= calls <= 10**9 else 0,
            "mix": clean_mix(row.get("mix")), "at": at}


def clean_mix(text) -> str:
    """The mix text 'Bash=3,Edit=2' when every item has that shape, else ''. At most 200 characters."""
    if not isinstance(text, str) or not text or len(text) > 200:
        return ""
    items = text.split(",")
    return text if all(MIX_ITEM_RX.fullmatch(i) for i in items) else ""


def mix_counts(row: dict) -> dict:
    """{tool: calls} for one log row: its mix when it has one, else its tool when it stands for calls."""
    if row["mix"]:
        out: dict = {}
        for item in row["mix"].split(","):
            m = MIX_ITEM_RX.fullmatch(item)
            if m:
                out[m.group(1)] = out.get(m.group(1), 0) + int(m.group(2))
        return out
    if row["calls"] > 0 and row["tool"] not in LIFECYCLE_TOOLS:
        return {row["tool"]: row["calls"]}
    return {}


def merge_log(old: list, new: list) -> list:
    """Add new rows to old, ignoring ids already held, keep ascending id order and the newest LOG_KEEP."""
    held = {r["id"] for r in old}
    out = old + [r for r in new if r["id"] not in held]
    out.sort(key=lambda r: r["id"])
    return out[-LOG_KEEP:]


def drop_purged(log_rows: list, sessions: list, sessions_cut: bool) -> list:
    """Remove the held rows that the board no longer holds, found from the session rows (read on every refresh):
    a row of a session that has no session row any more was purged (unless the session list was cut at its
    limit, when absence says nothing), and a row older than its session's start predates a purge that was
    followed by new writes of the same session."""
    by_key = {(s["agent"], s["session"]): s for s in sessions}
    keep = []
    for r in log_rows:
        s = by_key.get((r["agent"], r["session"]))
        if s is None:
            if sessions_cut:
                keep.append(r)
            continue
        if r["at"] < s["started_at"]:
            continue
        keep.append(r)
    return keep


# ---------------------------------------------------------------- per agent state


def strip(rows: list, now: int, minutes: int = STRIP_MINUTES) -> list:
    """Calls per minute for the last `minutes` minutes, oldest bucket first. A write stands for several
    calls (its `calls`), placed at the time of the write."""
    buckets = [0] * minutes
    for r in rows:
        age = now - r["at"]
        if age < 0:
            age = 0
        idx = minutes - 1 - age // STRIP_BUCKET_S
        if 0 <= idx < minutes:
            buckets[idx] += r["calls"]
    return buckets


def holds_healthy_lease(agent: str, queue: dict) -> Optional[bool]:
    """True when a task the agent holds has an unexpired lease, False when every one is expired or has no
    lease, None when the board has no lease table (nothing can be said)."""
    if not queue.get("leases_available"):
        return None
    mine = [r for r in queue["claimed"] if r.get("owner") == agent]
    return any(r["has_lease"] and not r["expired"] for r in mine)


def agent_activity(agent: str, sessions: list, log_rows: list, claimed: list, queue: dict, now: int,
                   quiet_s: int) -> Optional[dict]:
    """The activity block of one agent, or None when no row exists for it. `claimed` lists the ids of the
    claimed tasks it owns."""
    mine = [s for s in sessions if s["agent"] == agent]
    if not mine:
        return None
    cur = max(mine, key=lambda s: (s["last_seen"], s["started_at"], s["id"]))
    age = max(0, now - cur["last_seen"])
    rows = [r for r in log_rows if r["agent"] == agent]
    last_call = next((r for r in reversed(rows) if r["calls"] > 0 and r["tool"] not in LIFECYCLE_TOOLS), None)
    if cur["state"] == "ended":
        state = "idle"  # the session said it ended: a held task is the lease's business (expired-lease items), not "quiet"
    elif claimed:
        if age <= WORKING_S and cur["state"] == "working":
            state = "working"  # a turn that ended (Stop) is not work, however fresh the report
        elif age <= quiet_s:
            state = "quiet"
        else:
            healthy = holds_healthy_lease(agent, queue)
            state = "stuck?" if healthy else "quiet"
    else:
        state = "working" if (cur["state"] == "working" and age <= WORKING_S) else "idle"
    doing = None
    if cur["tool"] not in LIFECYCLE_TOOLS and cur["state"] != "ended":
        doing = {"tool": cur["tool"], "target": cur["target"], "outcome": cur["outcome"]}
    return {
        "state": state,
        "session": cur["session"],
        "session_state": cur["state"],
        "age_s": age,
        "task_id": cur["task_id"] or None,
        "calls": cur["tool_calls"],
        "doing": doing,
        "last_call": ({"tool": last_call["tool"], "target": last_call["target"], "outcome": last_call["outcome"],
                       "age_s": max(0, now - last_call["at"])} if last_call else None),
        "sessions": len(mine),
        "strip": strip(rows, now),
    }


def activity_for_agents(names: list, sessions: list, log_rows: list, tasks: list, queue: dict, now: int,
                        quiet_s: int) -> dict:
    held: dict = {}
    for t in tasks:
        if t["state"] == "claimed" and t.get("owner"):
            held.setdefault(t["owner"], []).append(t["id"])
    by_agent: dict = {}
    for r in log_rows:
        by_agent.setdefault(r["agent"], []).append(r)
    out = {}
    for name in names:
        out[name] = agent_activity(name, sessions, by_agent.get(name, []), held.get(name, []), queue, now, quiet_s)
    return out


def quiet_claims(activity: dict, tasks_by_id: dict, queue: dict, now: int) -> list:
    """The needs-attention items: a task is claimed, its lease is alive, and its owner's tool activity stopped."""
    hb = {r["id"]: r for r in queue.get("claimed", [])}
    out = []
    for name, a in sorted(activity.items()):
        if not a or a["state"] != "stuck?":
            continue
        for tid, t in sorted(tasks_by_id.items()):
            if t["state"] == "claimed" and t.get("owner") == name:
                row = hb.get(tid, {})
                out.append({"task_id": tid, "title": t["title"][:300], "owner": name, "quiet_s": a["age_s"],
                            "heartbeat_age_s": row.get("heartbeat_age_s")})
    out.sort(key=lambda x: -x["quiet_s"])
    return out


QUIET_RULE = ("claimed task whose owner's tool activity stopped for more than %d minutes while its lease is "
              "still alive; the hook is self-reported, so this says what was reported, not that the agent is stuck")


# ---------------------------------------------------------------- the agent screen


def active_estimate(rows: list) -> int:
    """Seconds of work, estimated: the sum of the gaps below GAP_S between consecutive rows of one session."""
    by_session: dict = {}
    for r in rows:
        if r["calls"] > 0:
            by_session.setdefault(r["session"], []).append(r["at"])
    total = 0
    for times in by_session.values():
        times.sort()
        for a, b in zip(times, times[1:]):
            if 0 <= b - a < GAP_S:
                total += b - a
    return total


def agent_detail(name: str, sessions: list, log_rows: list, tasks: list, now: int, tool: Optional[str],
                 outcome: Optional[str], task: Optional[str], limit: int, after: Optional[int]) -> dict:
    rows = [r for r in log_rows if r["agent"] == name]
    shown = [r for r in rows if (not tool or r["tool"] == tool or tool in mix_counts(r))
             and (not outcome or r["outcome"] == outcome) and (not task or r["task_id"] == task)]
    mix: dict = {}
    for r in shown:
        for k, v in mix_counts(r).items():
            mix[k] = mix.get(k, 0) + v
    if after is not None:
        page = [r for r in shown if r["id"] > after][:limit]  # follow: oldest first from the cursor
        cursor = page[-1]["id"] if page else max(after, rows[-1]["id"] if rows else 0)
        timeline = list(reversed(page))
    else:
        timeline = list(reversed(shown))[:limit]
        cursor = rows[-1]["id"] if rows else 0
    my_sessions = sorted([s for s in sessions if s["agent"] == name],
                         key=lambda s: (-s["last_seen"], s["id"]))[:DETAIL_SESSIONS]
    claimed = [{"id": t["id"], "title": t["title"][:300]} for t in tasks
               if t["state"] == "claimed" and t.get("owner") == name][:50]
    return {
        "agent": name,
        "now": now,
        "sessions": [dict(s, age_s=max(0, now - s["last_seen"])) for s in my_sessions],
        "claimed": claimed,
        "rows": [dict(r, age_s=max(0, now - r["at"])) for r in timeline],
        "rows_total": len(shown),
        "rows_held": len(rows),
        "cursor": cursor,
        "mix": sorted(mix.items(), key=lambda kv: (-kv[1], kv[0]))[:30],
        "active_s": active_estimate(rows),
        "tools": sorted({k for r in rows for k in (mix_counts(r) or {r["tool"]: 1})})[:60],
        "estimate_note": ("Active time is an estimate: the sum of the gaps shorter than %d s between the "
                          "writes the hook made, over the %d rows held. A write stands for the calls since "
                          "the one before it, so short pauses are counted as work and long ones are not."
                          % (GAP_S, len(rows))),
    }


# ---------------------------------------------------------------- the scorecard diagnostic


def _median(values: list) -> Optional[float]:
    if not values:
        return None
    s = sorted(values)
    mid = len(s) // 2
    return float(s[mid]) if len(s) % 2 else (s[mid - 1] + s[mid]) / 2.0


def active_per_task(facts: list, log_rows: list) -> dict:
    """A diagnostic, not a score: for each done task whose whole span the console watched, the estimated
    active seconds of its claimant between the claim and the done event. A task counts only when rows exist
    on both sides of the span's start and end (the first held row is not later than the claim and a row exists
    after it), so a task the console did not watch is left out, not counted as zero. n is shown everywhere and
    nothing is ranked."""
    by_agent: dict = {}
    for r in log_rows:
        by_agent.setdefault(r["agent"], []).append(r)
    per_task = []
    done_total = 0
    for f in facts:
        if f.task["state"] != "done" or f.done_ts is None or f.first_claim_ts is None or not f.claimant:
            continue
        done_total += 1
        rows = by_agent.get(f.claimant, [])
        if not rows or rows[0]["at"] > f.first_claim_ts or rows[-1]["at"] < f.done_ts:
            continue
        span = [r for r in rows if f.first_claim_ts <= r["at"] <= f.done_ts]
        if len(span) < 2:
            continue
        per_task.append((f.claimant, active_estimate(span)))

    def summary(values: list) -> dict:
        n = len(values)
        if n < MIN_TASKS:
            return {"n": n, "status": "insufficient", "message": "not enough data (n < %d)" % MIN_TASKS}
        return {"n": n, "status": "ok", "median_s": _median(values), "min_s": min(values), "max_s": max(values)}

    agents = sorted({a for a, _ in per_task})
    return {
        "overall": summary([v for _, v in per_task]),
        "agents": [dict(summary([v for a2, v in per_task if a2 == a]), agent=a) for a in agents],
        "done_tasks": done_total,
        "note": ("Diagnostic only: estimated active minutes of the claimant between claim and done, for the tasks "
                 "whose whole span the console watched. It depends on the agent's own hook, says nothing about "
                 "quality, and is listed by name, not ranked."),
    }
