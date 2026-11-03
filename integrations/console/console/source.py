"""Where the console reads rows from: psql over pgwire, or a deterministic demo.

Both sources answer the same few questions (tasks, events, decisions), so the server and the
model cannot tell them apart. The psql source is read-only by construction: it can only send
SELECT statements built from the fixed templates below, with integers it has validated.
"""

from __future__ import annotations

import os
import re
import selectors
import subprocess
import signal
import time
from typing import Optional
from urllib.parse import unquote

# ---------------------------------------------------------------- errors and scrubbing


class SourceError(Exception):
    """A failure reading the board. str(e) is safe to show to a browser: it is a fixed hint plus,
    for psql failures, the first stderr line with credentials scrubbed."""


class ConfigError(Exception):
    """The connection settings are unacceptable (for example a password in the URL)."""


_SCRUBBERS = [
    (re.compile(r"(?i)(password\s*=\s*)('[^']*'|\"[^\"]*\"|\S+)"), r"\1***"),
    (re.compile(r"(?i)(passfile\s*=\s*)\S+"), r"\1***"),
    (re.compile(r"(://[^:@/\s]*):[^@\s]*@"), r"\1:***@"),
]


def scrub(text: str, extra_secrets=()) -> str:
    """Remove anything that looks like a credential from one line of text."""
    out = text
    for rx, repl in _SCRUBBERS:
        out = rx.sub(repl, out)
    for s in extra_secrets:
        if s and len(s) >= 3:
            out = out.replace(s, "***")
    return out


def first_line(text: str, limit: int = 200) -> str:
    for line in text.splitlines():
        line = line.strip()
        if line:
            return line[:limit]
    return ""


# ---------------------------------------------------------------- connection settings

_URL_PASSWORD = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*://[^/?#@]*:[^/?#@]*@")


def check_url(url: str) -> str:
    """Return url unchanged if it carries no password, else raise ConfigError. Same rule as the
    board helper, but stricter: a URL with a password is refused, never moved to PGPASSWORD."""
    if not url:
        return url
    # libpq percent-decodes URI keywords and values, so "pass%77ord=x" is a password. Check the
    # text as given and every decoded form until it stops changing.
    forms = [url]
    for _ in range(8):
        nxt = unquote(forms[-1])
        if nxt == forms[-1]:
            break
        forms.append(nxt)
    else:
        raise ConfigError("the URL is percent-encoded too deeply")
    for form in forms:
        if re.search(r"(?i)pass\s*word\s*=", form) or _URL_PASSWORD.match(form):
            raise ConfigError("the URL must not carry a password; set PGPASSWORD or use ~/.pgpass")
        if any(c in form for c in "\0\r\n"):
            raise ConfigError("the URL has control characters")
    return url


def connection_url(cli_url: Optional[str], env=None) -> str:
    """--url wins over LATTICE_BOARD_URL; empty means 'use the PG* variables'."""
    env = os.environ if env is None else env
    return check_url(cli_url if cli_url else env.get("LATTICE_BOARD_URL", ""))


# ---------------------------------------------------------------- bounded subprocess


def run_bounded(argv: list, stdin_text: str, timeout: float, max_bytes: int, env=None):
    """Run argv (never through a shell) with a deadline and an output cap. Returns
    (returncode, stdout_bytes, stderr_bytes). Raises SourceError on timeout, oversize output or
    a missing program. The child is killed in the first two cases."""
    try:
        proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
            start_new_session=True,  # own process group, so a kill reaches anything psql spawned
        )
    except FileNotFoundError:
        raise SourceError("psql was not found; install the PostgreSQL client tools") from None
    except OSError as e:
        raise SourceError("could not start psql (%s)" % e.__class__.__name__) from None
    deadline = time.monotonic() + timeout
    out = bytearray()
    err = bytearray()
    sel = selectors.DefaultSelector()
    try:
        try:
            proc.stdin.write(stdin_text.encode("utf-8"))
            proc.stdin.close()
        except (BrokenPipeError, OSError):
            pass  # the child exited early; its exit status says why
        sel.register(proc.stdout, selectors.EVENT_READ, out)
        sel.register(proc.stderr, selectors.EVENT_READ, err)
        open_streams = 2
        while open_streams:
            left = deadline - time.monotonic()
            if left <= 0:
                raise SourceError("the database did not answer within %d seconds" % int(timeout))
            for key, _ in sel.select(timeout=min(left, 0.25)):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    sel.unregister(key.fileobj)
                    open_streams -= 1
                    continue
                key.data.extend(chunk)
                if len(out) > max_bytes:
                    raise SourceError("the database answer is larger than %d bytes" % max_bytes)
                if len(err) > 65536:
                    del err[65536:]
        try:
            rc = proc.wait(timeout=max(0.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            raise SourceError("the database did not answer within %d seconds" % int(timeout)) from None
        return rc, bytes(out), bytes(err)
    except BaseException:
        _kill_group(proc)
        raise
    finally:
        sel.close()
        for f in (proc.stdout, proc.stderr, proc.stdin):
            try:
                f.close()
            except Exception:
                pass


def _kill_group(proc) -> None:
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        try:
            proc.kill()
        except OSError:
            pass
    proc.wait()


# ---------------------------------------------------------------- psql source

TASK_COLUMNS = ("id", "title", "state", "owner", "priority", "parent_id", "created_by", "body",
                "created_at", "updated_at", "claim_seq", "done_seq", "created_seq")
EVENT_COLUMNS = ("id", "task_id", "author", "kind", "body", "created_at")
DECISION_COLUMNS = ("id", "task_id", "author", "body", "created_at")
LEASE_COLUMNS = ("task_id", "lease_until", "heartbeat_at", "attempts", "max_attempts", "not_before",
                 "last_error", "blocked_by")
LEASE_INTS = ("lease_until", "heartbeat_at", "attempts", "max_attempts", "not_before")
TASK_INTS = ("priority", "created_at", "updated_at", "claim_seq", "done_seq", "created_seq")
TASK_NULLS = ("owner", "parent_id")
EVENT_INTS = ("id", "created_at")
DECISION_INTS = ("id", "created_at")

# The only statements the console may send. Anything else is refused before psql starts.
# Used with fullmatch: "$" would also accept a trailing newline.
_SQL_ALLOWED = re.compile(
    r"SELECT [a-z_]+(?:, [a-z_]+)* FROM (?:tasks|events|decisions|task_leases)"
    r"(?: WHERE id > [0-9]{1,18})?"
    r" ORDER BY [a-z_]+(?: DESC)? LIMIT [0-9]{1,6};"
    # The experiment tables of `board bench`: the experiment list, or the rows of one experiment, whose
    # name is checked again against BENCH_NAME before it reaches the statement.
    r"|SELECT [a-z_]+(?:, [a-z_]+)* FROM bench_(?:experiments|arms|tasks|attempts)"
    r"(?: WHERE experiment_id = '[A-Za-z0-9_-]{1,48}')?"
    r" ORDER BY [a-z_]+(?: DESC)? LIMIT [0-9]{1,6};"
)
BENCH_NAME = re.compile(r"[A-Za-z0-9_-]{1,48}")
BENCH_EXPERIMENT_COLUMNS = ("id", "suite", "suite_sha", "seed", "repeats", "planned", "harness_version",
                            "created_by", "created_at")
BENCH_EXPERIMENT_INTS = ("seed", "repeats", "planned", "created_at")
BENCH_ARM_COLUMNS = ("arm", "agent", "config_sha")
BENCH_TASK_COLUMNS = ("task_key", "fixture_digest")
BENCH_ATTEMPT_COLUMNS = ("board_task_id", "arm", "task_key", "repeat_idx", "order_idx", "outcome", "flaky",
                         "tampered", "duration_ms", "cost_usd", "tokens", "grader_exit", "grader_runs",
                         "harness_version", "recorded_at", "summary")
BENCH_ATTEMPT_INTS = ("repeat_idx", "order_idx", "flaky", "tampered", "recorded_at")
BENCH_ATTEMPT_NULL_INTS = ("duration_ms", "tokens", "grader_exit")
BENCH_ATTEMPT_LIMIT = 20000
BENCH_EXPERIMENT_LIMIT = 200

# The one other statement: the database clock, which leases and retry delays are measured on.
_SQL_NOW = "SELECT CAST(extract(epoch FROM now()) AS bigint);"

MAX_INT = 2**53  # ids stay exact in JSON numbers up to here


def _int(name: str, value, lo: int, hi: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError("%s must be an integer" % name)
    if not lo <= value <= hi:
        raise ValueError("%s out of range" % name)
    return value


def build_select(columns: tuple, table: str, order: str, desc: bool, limit: int,
                 after: Optional[int] = None, experiment: Optional[str] = None) -> str:
    sql = "SELECT %s FROM %s" % (", ".join(columns), table)
    if after is not None:
        sql += " WHERE id > %d" % _int("after", after, 0, MAX_INT)
    if experiment is not None:
        if not BENCH_NAME.fullmatch(experiment):
            raise ValueError("not an experiment name")
        sql += " WHERE experiment_id = '%s'" % experiment
    sql += " ORDER BY %s%s LIMIT %d;" % (order, " DESC" if desc else "", _int("limit", limit, 1, 100000))
    if not _SQL_ALLOWED.fullmatch(sql):
        raise ValueError("refusing to send a statement that is not a plain SELECT")
    return sql


CONNECT_TRIES = 6
_TRANSIENT = ("server closed the connection unexpectedly", "connection reset", "connection refused", "timeout expired")
_NOT_TRANSIENT = ("fatal", "no password supplied", "ssl", "tls", "certificate", "password authentication")


def transient_connect_error(stderr: str) -> bool:
    """True for a connect-phase failure worth asking again about (see PsqlSource._run)."""
    low = stderr.lower()
    if "connection to server" not in low:
        return False
    if any(x in low for x in _NOT_TRANSIENT):
        return False
    return any(x in low for x in _TRANSIENT)


class PsqlSource:
    demo = False
    label = "database"

    def __init__(self, url: str = "", psql: Optional[str] = None, timeout: float = 10.0,
                 max_bytes: int = 8 * 1024 * 1024, env=None):
        self.url = check_url(url)
        self.psql = psql or os.environ.get("BOARD_PSQL") or "psql"
        self.timeout = timeout
        self.max_bytes = max_bytes
        self.env = dict(os.environ if env is None else env)
        # Messages in English whatever the user's locale: they are matched below. LC_ALL would win over
        # LC_MESSAGES, so it goes.
        self.env.pop("LC_ALL", None)
        self.env["LC_MESSAGES"] = "C"
        # Ask the server for a read-only session and a statement deadline, keeping any options the
        # user already set. (latticed ignores startup options today; PostgreSQL honours them.)
        mine = "-c default_transaction_read_only=on -c statement_timeout=8000"
        have = self.env.get("PGOPTIONS", "").strip()
        self.env["PGOPTIONS"] = (have + " " + mine).strip()

    # -- low level

    def argv(self) -> list:
        # -z -0: fields and records end in a NUL byte, which no text value can contain, so
        # agent-written text cannot shift columns. The password never appears here.
        a = [self.psql, "-X", "-q", "-A", "-t", "-z", "-0", "-v", "ON_ERROR_STOP=1",
             "-v", "VERBOSITY=terse"]
        if self.url:
            a += ["-d", self.url]
        return a

    def _run(self, sql: str) -> bytes:
        if not (_SQL_ALLOWED.fullmatch(sql) or sql == _SQL_NOW):
            raise ValueError("refusing to send a statement that is not a plain SELECT")
        for attempt in range(1, CONNECT_TRIES + 1):
            rc, out, err = run_bounded(self.argv(), sql + "\n", self.timeout, self.max_bytes, self.env)
            # The alpha server drops a share of new connections (issue 867). Every statement the console
            # sends is a SELECT, so asking again after a connect-phase failure cannot repeat a write. Never
            # retried: a wrong password, TLS and certificate errors, SQL errors, anything else.
            if rc == 2 and attempt < CONNECT_TRIES and transient_connect_error(err.decode("utf-8", "replace")):
                time.sleep(min(0.1 * 2 ** (attempt - 1), 1.0))
                continue
            break
        if rc != 0:
            secrets = [self.env.get("PGPASSWORD", "")]
            line = scrub(first_line(err.decode("utf-8", "replace")), secrets)
            hint = "check that latticed is running and LATTICE_BOARD_URL or PG* point to it"
            raise SourceError("the database query failed (%s)%s" % (hint, ": " + line if line else ""))
        return out

    def _rows(self, sql: str, columns: tuple, ints=(), nulls=()) -> list:
        return parse_rows(self._run(sql), columns, ints, nulls)

    # -- the questions

    def now(self) -> int:
        """The database clock when it answers, else this machine's clock (clock_source says which).
        Lease and retry times are database-clock seconds, so they are only exact in the first case."""
        try:
            out = self._run(_SQL_NOW).decode("utf-8", "replace").strip("\0\n ")
            self.clock_source = "database"
            return _to_int("now", out)
        except SourceError:
            self.clock_source = "client"
            return int(time.time())

    clock_source = "database"

    def leases(self, limit: int) -> tuple:
        """(rows, available). A board made before leases existed has no task_leases table: that is
        an empty answer, not an error."""
        sql = build_select(LEASE_COLUMNS, "task_leases", "task_id", False, limit + 1)
        try:
            rows = self._rows(sql, LEASE_COLUMNS, LEASE_INTS)
        except SourceError as e:
            if "task_leases" in str(e) and ("exist" in str(e) or "42P01" in str(e)):
                return [], False
            raise
        return rows[:limit], True

    def tasks(self, limit: int) -> tuple:
        sql = build_select(TASK_COLUMNS, "tasks", "created_seq", True, limit + 1)
        rows = self._rows(sql, TASK_COLUMNS, TASK_INTS, TASK_NULLS)
        truncated = len(rows) > limit
        rows = rows[:limit]
        rows.reverse()
        return rows, truncated

    def events_latest(self, limit: int) -> tuple:
        sql = build_select(EVENT_COLUMNS, "events", "id", True, limit + 1)
        rows = self._rows(sql, EVENT_COLUMNS, EVENT_INTS)
        truncated = len(rows) > limit
        rows = rows[:limit]
        rows.reverse()
        return rows, truncated

    def events_after(self, after: int, limit: int) -> list:
        return self._rows(build_select(EVENT_COLUMNS, "events", "id", False, limit, after),
                          EVENT_COLUMNS, EVENT_INTS)

    def decisions_latest(self, limit: int) -> list:
        rows = self._rows(build_select(DECISION_COLUMNS, "decisions", "id", True, limit),
                          DECISION_COLUMNS, DECISION_INTS)
        rows.reverse()
        return rows

    def decisions_after(self, after: int, limit: int) -> list:
        return self._rows(build_select(DECISION_COLUMNS, "decisions", "id", False, limit, after),
                          DECISION_COLUMNS, DECISION_INTS)

    # -- board bench

    def bench_experiments(self) -> tuple:
        """(rows, available): the experiments, newest first. A board made before `board bench` has no
        bench tables: that is an empty answer (available False), not an error."""
        sql = build_select(BENCH_EXPERIMENT_COLUMNS, "bench_experiments", "created_at", True, BENCH_EXPERIMENT_LIMIT)
        try:
            rows = self._rows(sql, BENCH_EXPERIMENT_COLUMNS, BENCH_EXPERIMENT_INTS)
        except SourceError as e:
            if "bench_experiments" in str(e) and ("exist" in str(e) or "42P01" in str(e)):
                return [], False
            raise
        return rows, True

    def bench_data(self, name: str):
        """The rows of one experiment in the shape of benchlib.report.parse_rows, or None when there is no
        such experiment."""
        if not BENCH_NAME.fullmatch(name):
            raise ValueError("not an experiment name")
        rows, _ = self.bench_experiments()
        exp = next((r for r in rows if r["id"] == name), None)
        if exp is None:
            return None
        arms = self._rows(build_select(BENCH_ARM_COLUMNS, "bench_arms", "arm", False, 64, experiment=name),
                          BENCH_ARM_COLUMNS)
        tasks = self._rows(build_select(BENCH_TASK_COLUMNS, "bench_tasks", "task_key", False, 1000, experiment=name),
                           BENCH_TASK_COLUMNS)
        att = self._rows(build_select(BENCH_ATTEMPT_COLUMNS, "bench_attempts", "order_idx", False,
                                      BENCH_ATTEMPT_LIMIT + 1, experiment=name),
                         BENCH_ATTEMPT_COLUMNS, BENCH_ATTEMPT_INTS + BENCH_ATTEMPT_NULL_INTS, BENCH_ATTEMPT_NULL_INTS)
        return bench_shape(exp, arms, tasks, att)


def parse_rows(out: bytes, columns: tuple, int_columns=(), null_columns=()) -> list:
    """Parse psql -A -t -z -0 output: a flat run of NUL-terminated values, len(columns) per row.
    A stream that does not divide into whole rows is an error, never a guess. psql prints a NULL
    as an empty value, so null_columns map "" to None."""
    n = len(columns)
    text = out.decode("utf-8", "replace")
    if not text:
        return []
    tokens = text.split("\0")
    if tokens[-1] in ("", "\n"):
        tokens.pop()
    else:
        tokens[-1] = tokens[-1].rstrip("\n")
    if len(tokens) % n != 0:
        raise SourceError("unexpected psql output (%d values do not fill rows of %d)" % (len(tokens), n))
    rows = []
    for i in range(0, len(tokens), n):
        row = {}
        for col, val in zip(columns, tokens[i:i + n]):
            if col in null_columns and val == "":
                row[col] = None
            elif col in int_columns:
                row[col] = _to_int(col, val)
            else:
                row[col] = val
        rows.append(row)
    return rows


def _to_int(col: str, val: str) -> int:
    try:
        return int(val)
    except ValueError:
        raise SourceError("unexpected psql output (column %s is not a number)" % col) from None


def bench_shape(exp: dict, arms: list, tasks: list, attempts: list) -> dict:
    """Rows from the bench tables in the shape benchlib.report.parse_rows returns."""
    truncated = len(attempts) > BENCH_ATTEMPT_LIMIT
    out = []
    for a in attempts[:BENCH_ATTEMPT_LIMIT]:
        cost = a["cost_usd"]
        try:
            cost = float(cost) if cost not in ("", None) else None
        except ValueError:
            raise SourceError("unexpected psql output (cost_usd is not a number)") from None
        out.append({"board_task_id": a["board_task_id"], "arm": a["arm"], "task_key": a["task_key"],
                    "repeat": a["repeat_idx"], "order": a["order_idx"], "outcome": a["outcome"],
                    "flaky": bool(a["flaky"]), "tampered": a["tampered"], "duration_ms": a["duration_ms"],
                    "cost_usd": cost, "tokens": a["tokens"], "grader_exit": a["grader_exit"],
                    "grader_runs": a["grader_runs"], "harness": a["harness_version"],
                    "recorded_at": a["recorded_at"], "summary": a["summary"]})
    return {"experiment": {"name": exp["id"], "suite": exp["suite"], "suite_sha": exp["suite_sha"],
                           "seed": exp["seed"], "repeats": exp["repeats"], "planned": exp["planned"],
                           "harness": exp["harness_version"], "created_by": exp["created_by"],
                           "created_at": exp["created_at"]},
            "arms": [{"arm": r["arm"], "agent": r["agent"], "config_sha": r["config_sha"]} for r in arms],
            "tasks": [{"task": r["task_key"], "fixture_digest": r["fixture_digest"]} for r in tasks],
            "attempts": out, "truncated": truncated}
