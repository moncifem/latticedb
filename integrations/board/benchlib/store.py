"""The only code in `board bench` that reaches the database: every call is the `board` helper, so the
SQL, the fence rule and the retry rules exist in one place (the helper) and nothing here knows any SQL."""

from __future__ import annotations

import os
import subprocess

from . import report as reportmod


class BoardError(Exception):
    def __init__(self, message, rc=1):
        super().__init__(message)
        self.rc = rc


class Board:
    def __init__(self, path=None, author=None):
        here = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
        self.path = path or os.environ.get("BOARD_BENCH_BOARD") or os.path.join(here, "board")
        self.env = dict(os.environ)
        self.env["BOARD_AUTHOR"] = author or os.environ.get("BOARD_AUTHOR") or "bench"

    def call(self, *args, timeout=180):
        """(returncode, stdout, stderr); never raises for a failing helper."""
        try:
            p = subprocess.run([self.path, *args], env=self.env, stdin=subprocess.DEVNULL, capture_output=True,
                               text=True, timeout=timeout, errors="replace")
        except subprocess.TimeoutExpired:
            return 124, "", "the board helper did not answer in %ss" % timeout
        except OSError as e:
            return 127, "", "cannot run the board helper: %s" % (e.strerror or e)
        return p.returncode, p.stdout, p.stderr

    def must(self, *args, timeout=180):
        rc, out, err = self.call(*args, timeout=timeout)
        if rc != 0:
            first = (err.strip().splitlines() or ["failed"])[-1]
            raise BoardError("board %s: %s" % (args[0], first[:300]), rc)
        return out

    # -- writes
    def register(self, name, suite, sha, seed, repeats, planned, harness):
        self.must("bench-experiment", name, "--suite", suite, "--suite-sha", sha, "--seed", str(seed),
                  "--repeats", str(repeats), "--planned", str(planned), "--harness", harness)

    def add_arm(self, exp, arm, agent, config_sha):
        self.must("bench-arm", exp, arm, "--agent", agent, "--config-sha", config_sha)

    def add_task(self, exp, task, digest):
        self.must("bench-task", exp, task, "--digest", digest)

    def add_board_task(self, title, body):
        out = self.must("add", title, "--body", body, "--needs-approval", "--max-attempts", "3")
        tid = out.strip().splitlines()[-1] if out.strip() else ""
        if not tid or any(c.isspace() for c in tid):
            raise BoardError("board add did not print a task id")
        return tid

    def plan(self, exp, arm, task, board_task, repeat, order, harness):
        self.must("bench-plan", exp, arm, task, board_task, "--repeat", str(repeat), "--order", str(order),
                  "--harness", harness)

    def approve(self, board_task):
        return self.call("approve", board_task)[0] == 0

    def cancel(self, board_task):
        return self.call("cancel", board_task, "bench cancelled")[0] == 0

    def record(self, board_task, fence, fields):
        """Run bench-record; (rc, message). rc 3 means the fence refused."""
        args = ["bench-record", board_task, "--fence", str(fence), "--outcome", fields["outcome"],
                "--experiment", fields["exp"], "--arm", fields["arm"], "--task", fields["task"],
                "--flaky", "1" if fields.get("flaky") else "0", "--tampered", str(int(fields.get("tampered") or 0))]
        for opt, key in (("--duration-ms", "duration_ms"), ("--tokens", "tokens"), ("--grader-exit", "grader_exit")):
            if fields.get(key) is not None:
                args += [opt, str(int(fields[key]))]
        if fields.get("cost_usd") is not None:
            args += ["--cost", "%.6f" % fields["cost_usd"]]
        if fields.get("grader_runs"):
            args += ["--grader-runs", fields["grader_runs"]]
        if fields.get("summary"):
            args += ["--summary", fields["summary"]]
        rc, out, err = self.call(*args)
        if rc == 0:
            return rc, out.strip()
        return rc, (err.strip().splitlines() or ["failed"])[-1]

    # -- reads
    def rows(self, exp):
        return reportmod.parse_rows(self.must("bench-rows", exp))

    def list_experiments(self):
        out = []
        for line in self.must("bench-list").splitlines():
            p = line.split("\t")
            if len(p) == 5:
                out.append({"name": p[0], "suite": p[1], "repeats": int(p[2]), "planned": int(p[3]), "created_at": int(p[4])})
        return out
