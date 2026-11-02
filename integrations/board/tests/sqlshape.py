"""What SQL does each write command of the board helper send? (no database involved)

For every command that changes a task, check, on the script the helper sends to psql, that the
compare-and-set that protects the write is in the SAME script and that no write sits outside it.
The expectations below were derived by hand from the rules in integrations/board/CONVENTIONS.md
(a write needs the owner and the fence, a reap needs the claim and the lease past, and so on), not
by running the helper and copying what it sent. test_sqlshape.py runs check_all on the real helper
(it must find nothing) and on copies with one protection removed (it must find something each time).

A script has this shape: counter bump, database time, `\\set ok f`, a guard statement
`SELECT (...) AS ok ... \\gset`, `\\if :ok`, the writes, `\\endif`, a result SELECT. A write is
protected when it is inside an `\\if :ok` block that comes after a guard statement.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
FAKE = os.path.join(HERE, "fakepsql.py")

HELD = (r"state = 'claimed' AND coalesce\(owner, ''\) = :'author' "
        r"AND \(claim_seq = :fence OR :fence < 0\)")

# name -> (command line, rules, regexes that must each match some guard statement,
#          regexes that must each match some statement inside the guarded block)
PATHS = {
    "heartbeat": (["heartbeat", "t1", "--fence", "5"],
                  [("AS until", "t\t1")],
                  [HELD], [r"UPDATE task_leases SET lease_until = :dbnow \+ :lease"]),
    "note": (["note", "t1", "hi", "--fence", "5"],
             [(r"'note', :'text'", "t\t5")],
             [r":fence < 0 OR \(state = 'claimed' AND coalesce\(owner, ''\) = :'author' "
              r"AND claim_seq = :fence\)"],
             [r"INSERT INTO events .* 'note'"]),
    "done": (["done", "t1", "--fence", "5"],
             [(r"SET state = 'done'", "t\t9")],
             [r"\(state = 'claimed' AND coalesce\(owner, ''\) = :'author' "
              r"AND \(claim_seq = :fence OR :fence < 0\)\) OR \(state = 'open' AND :fence < 0\)"],
             [r"UPDATE tasks SET state = 'done', done_seq = :seq",
              r"INSERT INTO events .* 'done'.* AND done_seq = :seq",
              r"UPDATE task_leases SET lease_until = 0"]),
    "fail": (["fail", "t1", "why", "--fence", "5", "--retry-in", "1"],
             [(r"'fail', :'text'", "t\tf")],
             [HELD],
             [r"attempts \+ 1 >= max_attempts", r"UPDATE tasks SET state = 'dead'",
              r"UPDATE tasks SET state = 'open'", r"INSERT INTO events .* 'fail'"]),
    "release": (["release", "t1", "--fence", "5"],
                [(r"'release', ''", "t\t4")],
                [HELD],
                [r"UPDATE tasks SET state = 'open'", r"UPDATE task_leases SET lease_until = 0"]),
    "cancel": (["cancel", "t1"],
               [(r"SET state = 'cancelled'", "t\t50")],
               [r"state = 'open' OR state = 'claimed' OR state = 'awaiting_approval'"],
               [r"UPDATE tasks SET state = 'cancelled'"]),
    "approve": (["approve", "t1"],
                [(r"'approve', :'text'", "t\t41"), (r"SELECT state, coalesce\(owner", "awaiting_approval\t\t0")],
                [r"\(state = 'awaiting_approval'\) AS ok"],
                [r"UPDATE tasks SET state = 'open'"]),
    "block": (["block", "ta", "tb"],
              [(r"SELECT \(blocked_by = :'old'\)", "t")],
              [r"blocked_by = :'old'"],
              [r"UPDATE task_leases SET blocked_by = :'new'"]),
    "reap": (["reap"],
             [(r"'expired'", "t\tf")],
             [r"state = 'claimed' AND claim_seq = :fence",
              r"lease_until > 0 AND lease_until < :dbnow"],
             [r"UPDATE tasks SET state = 'dead'", r"UPDATE tasks SET state = 'open'",
              r"attempts \+ 1 >= max_attempts", r"INSERT INTO events .* 'expired'"]),
    "claim": (["claim", "t1"],
              [(r"UPDATE tasks SET owner = :'author', state = 'claimed'", "t\talice\tclaimed\t17\tt")],
              [r"not_before <= :dbnow"],
              [r"UPDATE tasks SET owner = :'author', state = 'claimed', claim_seq = :seq, "
               r"updated_at = :ts WHERE id = :'task' AND owner IS NULL AND state = 'open'",
               r"INSERT INTO events .* 'claim'"]),
}

# canned answers to the reads each command makes before its write transaction
COMMON_READS = [
    (r"SELECT state, coalesce\(owner", "claimed\talice\t5"),
    (r"SELECT task_id FROM task_leases WHERE task_id =", "t1"),
    (r"SELECT attempts FROM task_leases", "0"),
    (r"SELECT blocked_by FROM task_leases", ""),
    (r"SELECT id FROM tasks WHERE id IN", "tb"),
    (r"AS bigint\);", "5000"),
    (r"lease_until > 0 AND lease_until < :dbnow\s+ORDER BY", "t1\t0"),
]


def _statements(sql):
    """Split a script into (kind, conds, text): kind is 'meta' (a backslash line), 'guard' (a
    statement ending in \\gset), 'write' (UPDATE or INSERT) or 'other'. conds is the tuple of the
    open \\if conditions around it, outermost first (for example (':ok', ':exhausted'))."""
    out, stack, cur = [], [], []
    for line in sql.splitlines():
        stripped = line.strip()
        if not cur and stripped.startswith("\\") and not stripped.endswith("\\gset"):
            if stripped.startswith("\\if"):
                out.append(("meta", tuple(stack), stripped))
                stack.append(stripped[3:].strip())
            elif stripped.startswith("\\endif"):
                if stack:
                    stack.pop()
                out.append(("meta", tuple(stack), stripped))
            else:
                out.append(("meta", tuple(stack), stripped))
            continue
        if not stripped:
            continue
        cur.append(stripped)
        if stripped.endswith(";") or stripped.endswith("\\gset"):
            text = " ".join(cur)
            cur = []
            if text.endswith("\\gset"):
                kind = "guard"
            elif re.match(r"(UPDATE|INSERT)\b", text) and not text.startswith("UPDATE counters"):
                kind = "write"
            else:
                kind = "other"
            out.append((kind, tuple(stack), text))
    return out


def check_script(name, sql):
    """Problems with one write script, as a list of strings (empty when it is protected)."""
    _, _, guards, inner = PATHS[name]
    problems = []
    stmts = _statements(sql)
    first_guard = next((i for i, s in enumerate(stmts) if s[0] == "guard" and "AS ok" in s[2]), None)
    if first_guard is None:
        return ["%s: no guard statement (SELECT ... AS ok ... \\gset) in the script" % name]
    for rx in guards:
        if not any(s[0] == "guard" and re.search(rx, s[2]) for s in stmts):
            problems.append("%s: no guard matches %s" % (name, rx))
    writes = [(i, s) for i, s in enumerate(stmts) if s[0] == "write"]
    if not writes:
        problems.append("%s: the script writes nothing" % name)
    for i, s in writes:
        if i < first_guard:
            problems.append("%s: a write comes before the guard: %s" % (name, s[2][:70]))
        elif ":ok" not in s[1]:
            problems.append("%s: a write is outside the \\if :ok block: %s" % (name, s[2][:70]))
    for rx in inner:
        if not any(s[0] == "write" and ":ok" in s[1] and re.search(rx, s[2]) for s in stmts) and \
                not any(s[0] == "guard" and re.search(rx, s[2]) for s in stmts):
            problems.append("%s: no guarded statement matches %s" % (name, rx))
    return problems


def _run(board, args, rules, tmp):
    psql = os.path.join(tmp, "psql")
    with open(psql, "w") as f:
        f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, FAKE))
    os.chmod(psql, 0o755)
    log = os.path.join(tmp, "calls.jsonl")
    if os.path.exists(log):
        os.remove(log)
    counts = log + ".counts"
    if os.path.exists(counts):
        os.remove(counts)
    rules_path = os.path.join(tmp, "rules.json")
    with open(rules_path, "w") as f:
        json.dump([{"match": m, "out": o} for m, o in rules], f)
    state = os.path.join(tmp, "state")
    os.makedirs(state, mode=0o700, exist_ok=True)
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": tmp, "TMPDIR": tmp,
           "BOARD_PSQL": psql, "BOARD_STATE_DIR": state, "BOARD_AUTHOR": "alice",
           "FAKE_LOG": log, "FAKE_RULES": rules_path}
    subprocess.run(["sh", board, *args], env=env, capture_output=True, text=True, timeout=60)
    if not os.path.exists(log):
        return []
    with open(log) as f:
        return [json.loads(line) for line in f if line.strip()]


def check_all(board):
    """Run every write path of the helper at `board` and return all problems found."""
    problems = []
    tmp = tempfile.mkdtemp(prefix="sqlshape-")
    try:
        for name, (args, rules, _, _) in PATHS.items():
            own = list(rules) + COMMON_READS
            calls = _run(board, args, own, tmp)
            marker = rules[0][0]
            tx = [c for c in calls if re.search(marker, c["sql"], re.S) and "BEGIN" in c["sql"]]
            if not tx:
                problems.append("%s: the helper sent no write transaction" % name)
                continue
            problems.extend(check_script(name, tx[0]["sql"]))
    finally:
        shutil.rmtree(tmp, True)
    return problems
