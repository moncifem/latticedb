"""Fixes from the independent review of the first version: each has a test here."""

import json
import os
import socket
import stat
import tempfile
import threading
import time
import unittest

import helpers  # noqa: F401
from helpers import FakeSource, event, small_board, task

from console import model, source
from console import server as server_mod
from console.server import ConsoleServer
from console.service import BoardService
from console.source import ConfigError, PsqlSource, SourceError, check_url, run_bounded
from test_server import EXPECTED_CSP, Running


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


# ---------------------------------------------------------------- 1. encoded password keywords


class EncodedPasswordTests(unittest.TestCase):
    def test_percent_encoded_keywords_are_refused(self):
        for url in ("postgresql://h/db?pass%77ord=x", "postgresql://h/db?%70assword=x",
                    "postgresql://h/db?%50ASSWORD=x", "postgresql://h/db?%70%61%73%73%77%6f%72%64=x",
                    "postgresql://h/db?user=a&pass%77ord=x", "host=h pass%77ord=x",
                    "postgresql://h/db?password%3Dx", "postgresql://dev:%70w@h/db"):
            with self.assertRaises(ConfigError, msg=url):
                check_url(url)

    def test_double_encoding_is_followed(self):
        with self.assertRaises(ConfigError):
            check_url("postgresql://h/db?%2570assword=x")
        with self.assertRaises(ConfigError):
            check_url("postgresql://h/db?%252570assword=x")

    def test_endless_encoding_is_refused(self):
        nested = "%41"
        for _ in range(12):
            nested = nested.replace("%", "%25")
        url = "postgresql://h/db?x=" + nested
        with self.assertRaises(ConfigError):
            check_url(url)

    def test_decoded_control_characters_are_refused(self):
        with self.assertRaises(ConfigError):
            check_url("postgresql://h/db?application_name=a%0a-c")

    def test_harmless_encoding_is_accepted(self):
        for url in ("postgresql://dev@h/db?application_name=a%20b", "postgresql://h/db?options=-c%20x%3D1"):
            self.assertEqual(check_url(url), url)

    def test_error_does_not_echo_the_url(self):
        try:
            check_url("postgresql://h/db?pass%77ord=hunter2")
        except ConfigError as e:
            self.assertNotIn("hunter2", str(e))


# ---------------------------------------------------------------- 2. load and failure handling


class BackoffTests(unittest.TestCase):
    def make(self):
        tasks, events, decisions = small_board()
        src = FakeSource(tasks, events, decisions)
        clock = Clock()
        return src, clock, BoardService(src, ttl=0.0, clock=clock)

    def test_no_new_read_while_the_failure_is_fresh(self):
        src, clock, s = self.make()
        src.error = SourceError("down")
        with self.assertRaises(SourceError):
            s.snapshot()
        n = len(src.calls)
        for _ in range(20):
            clock.t += 0.05
            with self.assertRaises(SourceError):
                s.snapshot()
        self.assertEqual(len(src.calls), n)  # psql was not started again

    def test_reads_resume_after_the_backoff(self):
        src, clock, s = self.make()
        src.error = SourceError("down")
        with self.assertRaises(SourceError):
            s.snapshot()
        src.error = None
        clock.t += 1.0
        with self.assertRaises(SourceError):
            s.snapshot()
        clock.t += 1.5
        self.assertEqual(s.snapshot().head, 8)

    def test_a_second_failure_starts_a_new_backoff(self):
        src, clock, s = self.make()
        src.error = SourceError("down")
        with self.assertRaises(SourceError):
            s.snapshot()
        clock.t += 2.5
        n = len(src.calls)
        with self.assertRaises(SourceError):
            s.snapshot()
        self.assertGreater(len(src.calls), n)
        n = len(src.calls)
        with self.assertRaises(SourceError):
            s.snapshot()
        self.assertEqual(len(src.calls), n)

    def test_http_requests_do_not_hammer_a_failing_source(self):
        src = FakeSource(*small_board())
        src.error = SourceError("down")
        srv = Running(src)
        try:
            for _ in range(15):
                self.assertEqual(srv.request("GET", "/api/now")[0], 502)
            self.assertLessEqual(len(src.calls), 2)
        finally:
            srv.stop()


class ScorecardCacheTests(unittest.TestCase):
    def setUp(self):
        self.s = BoardService(FakeSource(*small_board()), ttl=1000.0)

    def test_cache_is_bounded(self):
        for i in range(60):
            self.s.scorecard_payload((i * 600, i * 600 + 600), None, None)
        self.assertLessEqual(len(self.s._cache), 16)

    def test_recent_entries_survive_eviction(self):
        keep = self.s.scorecard_payload((0, 600), None, None)
        for i in range(1, 12):
            self.s.scorecard_payload((i * 600, i * 600 + 600), None, None)
            self.assertIs(self.s.scorecard_payload((0, 600), None, None), keep)

    def test_window_edges_are_rounded_to_a_minute(self):
        a = self.s.scorecard_payload((61, 3700), None, None)
        b = self.s.scorecard_payload((119, 3719), None, None)
        self.assertIs(a, b)
        self.assertEqual(a["overall"]["window"], {"from": 60, "to": 3660})

    def test_none_edges_stay_open(self):
        a = self.s.scorecard_payload((None, 90), None, None)
        self.assertEqual(a["overall"]["window"], {"from": None, "to": 60})


class ConnectionLimitTests(unittest.TestCase):
    def test_handler_has_a_timeout(self):
        srv = ConsoleServer(BoardService(FakeSource()), 0)
        try:
            self.assertEqual(srv.RequestHandlerClass.timeout, 10)
            self.assertEqual(server_mod.REQUEST_TIMEOUT_S, 10)
        finally:
            srv.server_close()

    def test_an_idle_connection_is_dropped(self):
        old = server_mod.REQUEST_TIMEOUT_S
        server_mod.REQUEST_TIMEOUT_S = 0.4
        try:
            srv = Running(FakeSource(*small_board()))
        finally:
            server_mod.REQUEST_TIMEOUT_S = old
        try:
            s = socket.create_connection(("127.0.0.1", srv.port), timeout=5)
            t0 = time.monotonic()
            data = s.recv(1024)  # we send nothing; the server must give up on us
            self.assertEqual(data, b"")
            self.assertLess(time.monotonic() - t0, 4)
            s.close()
        finally:
            srv.stop()

    def test_connections_beyond_the_cap_get_503_and_slots_come_back(self):
        service = BoardService(FakeSource(*small_board()), ttl=0.0)
        server = ConsoleServer(service, 0, max_connections=2)
        t = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        t.start()
        idle = []
        try:
            for _ in range(2):
                idle.append(socket.create_connection(("127.0.0.1", server.port), timeout=5))
                time.sleep(0.15)
            third = socket.create_connection(("127.0.0.1", server.port), timeout=5)
            data = b""
            while True:
                chunk = third.recv(4096)
                if not chunk:
                    break
                data += chunk
            third.close()
            head = data.split(b"\r\n\r\n")[0].decode()
            self.assertTrue(head.startswith("HTTP/1.1 503"), head)
            self.assertIn("Content-Security-Policy: " + EXPECTED_CSP, head)
            self.assertIn("Retry-After", head)
            self.assertEqual(json.loads(data.split(b"\r\n\r\n")[1]), {"error": "too many connections"})
            for s in idle:
                s.close()
            deadline = time.monotonic() + 5
            status = None
            while time.monotonic() < deadline:  # slots are released when the idle sockets end
                s = socket.create_connection(("127.0.0.1", server.port), timeout=5)
                s.sendall(b"GET /api/now HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n\r\n" % server.port)
                status = s.recv(20)
                s.close()
                if status.startswith(b"HTTP/1.1 200"):
                    break
                time.sleep(0.1)
            self.assertTrue(status.startswith(b"HTTP/1.1 200"), status)
        finally:
            for s in idle:
                s.close()
            server.shutdown()
            server.server_close()
            t.join(5)


# ---------------------------------------------------------------- 3. fullmatch


class TrailingNewlineTests(unittest.TestCase):
    def setUp(self):
        self.srv = Running(FakeSource(*small_board()))
        self.addCleanup(self.srv.stop)

    def status(self, path):
        return self.srv.request("GET", path)[0]

    def test_validators_refuse_a_trailing_newline(self):
        self.assertEqual(self.status("/api/feed?agent=alice%0a"), 400)
        self.assertEqual(self.status("/api/feed?task=tparent%0a"), 400)
        self.assertEqual(self.status("/api/task?id=tparent%0a"), 400)
        self.assertEqual(self.status("/api/feed?after=5%0a"), 400)
        self.assertEqual(self.status("/api/feed?limit=5%0a"), 400)
        self.assertEqual(self.status("/api/scorecard?from=5%0a"), 400)
        self.assertEqual(self.status("/api/scorecard?agent=bob%0a"), 400)

    def test_without_the_newline_they_pass(self):
        self.assertEqual(self.status("/api/feed?agent=alice&task=tparent&after=5&limit=5"), 200)

    def test_regexes_have_no_dollar_anchor(self):
        for rx in (server_mod._ID_RE, server_mod._NAME_RE, server_mod._INT_RE):
            self.assertFalse(rx.pattern.endswith("$"))
            self.assertIsNone(rx.fullmatch("abc\n" if rx is not server_mod._INT_RE else "12\n"))

    def test_sql_guard_refuses_a_trailing_newline(self):
        ok = "SELECT id FROM events ORDER BY id LIMIT 1;"
        self.assertTrue(source._SQL_ALLOWED.fullmatch(ok))
        self.assertFalse(source._SQL_ALLOWED.fullmatch(ok + "\n"))
        self.assertFalse(source._SQL_ALLOWED.fullmatch(ok + "\nDROP TABLE tasks;"))

    def test_sql_guard_in_the_adapter_refuses_a_trailing_newline(self):
        s = PsqlSource("", psql="/nonexistent/psql", env={})
        with self.assertRaises(ValueError):
            s._rows("SELECT id FROM events ORDER BY id LIMIT 1;\n", source.EVENT_COLUMNS)


# ---------------------------------------------------------------- 5. process group and server-side limits

FAKE_PSQL = r"""#!/bin/sh
printf '%s' "${PGOPTIONS:-}" > "$FAKE_DIR/pgoptions"
cat > /dev/null
case "$FAKE_MODE" in
  spawn) sleep 60 & echo $! > "$FAKE_DIR/child.pid"; wait ;;
  *) : ;;
esac
"""


class ChildProcessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.psql = os.path.join(self.tmp.name, "psql")
        with open(self.psql, "w") as f:
            f.write(FAKE_PSQL)
        os.chmod(self.psql, os.stat(self.psql).st_mode | stat.S_IXUSR)

    def env(self, mode="ok", **more):
        e = {"FAKE_DIR": self.tmp.name, "FAKE_MODE": mode, "PATH": os.environ.get("PATH", "")}
        e.update(more)
        return e

    def alive(self, pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        # a zombie that was not reaped still answers kill(0); ps knows better
        import subprocess
        out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout
        return bool(out.strip()) and not out.strip().startswith("Z")

    def test_timeout_kills_the_whole_process_group(self):
        s = PsqlSource("", psql=self.psql, env=self.env("spawn"), timeout=1.0)
        with self.assertRaises(SourceError):
            s.events_after(0, 10)
        with open(os.path.join(self.tmp.name, "child.pid")) as f:
            pid = int(f.read().strip())
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and self.alive(pid):
            time.sleep(0.1)
        self.assertFalse(self.alive(pid), "a grandchild of psql outlived the timeout")

    def test_child_runs_in_its_own_session(self):
        rc, out, _ = run_bounded(["/bin/sh", "-c", "ps -o pid=,pgid= -p $$"], "", 5, 1000)
        pid, pgid = out.split()
        self.assertEqual(pid, pgid)  # it leads its own group, so killpg cannot hit us

    def test_read_only_and_statement_timeout_options_are_requested(self):
        PsqlSource("", psql=self.psql, env=self.env()).events_after(0, 10)
        with open(os.path.join(self.tmp.name, "pgoptions")) as f:
            opts = f.read()
        self.assertIn("-c default_transaction_read_only=on", opts)
        self.assertIn("-c statement_timeout=8000", opts)

    def test_existing_pgoptions_are_kept(self):
        PsqlSource("", psql=self.psql, env=self.env(PGOPTIONS="-c search_path=board")).events_after(0, 10)
        with open(os.path.join(self.tmp.name, "pgoptions")) as f:
            opts = f.read()
        self.assertTrue(opts.startswith("-c search_path=board "), opts)
        self.assertIn("default_transaction_read_only=on", opts)

    def test_options_never_appear_in_argv(self):
        argv = PsqlSource("", psql=self.psql, env=self.env()).argv()
        self.assertFalse([a for a in argv if "read_only" in a or "statement_timeout" in a])


# ---------------------------------------------------------------- 6. every error has the headers


class ErrorPathTests(unittest.TestCase):
    def setUp(self):
        self.srv = Running(FakeSource(*small_board()))
        self.addCleanup(self.srv.stop)

    def raw(self, payload):
        s = socket.create_connection(("127.0.0.1", self.srv.port), timeout=5)
        s.sendall(payload)
        data = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        s.close()
        return data

    def check(self, data, status):
        head, _, body = data.partition(b"\r\n\r\n")
        text = head.decode("latin-1")
        self.assertTrue(text.startswith("HTTP/1."), text[:60])
        self.assertIn(" %d " % status, text.split("\r\n")[0])
        self.assertIn("Content-Security-Policy: " + EXPECTED_CSP, text)
        self.assertIn("X-Content-Type-Options: nosniff", text)
        self.assertIn("Referrer-Policy: no-referrer", text)
        self.assertIn("Cache-Control: no-store", text)
        self.assertNotIn(b"<html", body.lower())  # no HTML error page
        self.assertIn("error", json.loads(body.decode()))

    def test_unknown_method_is_501_with_headers(self):
        status, resp, data = self.srv.request("PROPFIND", "/")
        self.assertEqual(status, 501)
        self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP)
        self.assertEqual(resp.getheader("Cache-Control"), "no-store")
        self.assertIn("error", json.loads(data))

    def test_garbage_request_line(self):
        host = b"Host: 127.0.0.1:%d\r\n\r\n" % self.srv.port
        self.check(self.raw(b"THIS IS NOT HTTP\r\n" + host), 400)
        self.check(self.raw(b"GET\r\n" + host), 400)
        self.check(self.raw(b"\x00\x01\x02 / HTTP/1.1\r\n" + host), 501)  # an unknown method

    def test_unsupported_http_version(self):
        self.check(self.raw(b"GET / HTTP/9.9\r\nHost: 127.0.0.1:%d\r\n\r\n" % self.srv.port), 505)

    def test_overlong_request_line_is_414_with_headers(self):
        self.check(self.raw(b"GET /" + b"a" * 70000 + b" HTTP/1.1\r\nHost: x\r\n\r\n"), 414)

    def test_too_many_headers_is_431_with_headers(self):
        headers = b"".join(b"X-%d: y\r\n" % i for i in range(150))
        self.check(self.raw(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n" % self.srv.port + headers + b"\r\n"), 431)

    def test_error_bodies_are_short_fixed_text(self):
        data = self.raw(b"BAD LINE\r\n\r\n")
        self.assertLess(len(data), 1500)


# ---------------------------------------------------------------- 7. answer size


BIG = "\U0001F600" * 4096  # 4096 non-BMP characters: 49,152 bytes once JSON-escaped to ASCII


def big_source(n=500):
    tasks, events, decisions = small_board()
    for i in range(n):
        events.append(event(9 + i, "tparent", "alice", "note", 3000 + i, BIG))
    return FakeSource(tasks, events, decisions)


class AnswerSizeTests(unittest.TestCase):
    def setUp(self):
        self.srv = Running(big_source())
        self.addCleanup(self.srv.stop)

    def test_a_full_page_of_worst_case_events_fits_the_cap(self):
        status, resp, data = self.srv.request("GET", "/api/feed?limit=500")
        self.assertEqual(status, 200)
        self.assertLess(len(data), server_mod.MAX_BODY)
        d = json.loads(data)
        self.assertLess(len(d["events"]), 500)  # it shrank instead of failing
        self.assertTrue(d["has_more"])
        self.assertEqual([e["id"] for e in d["events"]], sorted((e["id"] for e in d["events"]), reverse=True))
        self.assertEqual(d["events"][0]["id"], 508)  # the newest events are the ones kept

    def test_older_pages_cover_the_rest(self):
        seen, before = set(), None
        for _ in range(100):
            q = "/api/feed?limit=500" + ("&before=%d" % before if before else "")
            d = json.loads(self.srv.request("GET", q)[2])
            seen.update(e["id"] for e in d["events"])
            if not d["has_more"] or not d["events"]:
                break
            before = d["events"][-1]["id"]
        self.assertEqual(seen, set(range(1, 509)))

    def test_following_the_feed_never_skips_and_always_advances(self):
        cursor, seen, rounds = 0, [], 0
        while True:
            rounds += 1
            self.assertLess(rounds, 200)
            status, resp, data = self.srv.request("GET", "/api/feed?limit=500&after=%d" % cursor)
            self.assertEqual(status, 200)
            self.assertLess(len(data), server_mod.MAX_BODY)
            d = json.loads(data)
            if d["events"]:
                self.assertGreater(d["cursor"], cursor)  # progress on every non-empty poll
            seen.extend(sorted(e["id"] for e in d["events"]))
            if d["cursor"] == d["head"] and not d["has_more"]:
                break
            self.assertGreater(d["cursor"], cursor)
            cursor = d["cursor"]
        self.assertEqual(seen, list(range(1, 509)))
        self.assertGreater(rounds, 2)  # it really needed several polls

    def test_task_answer_caps_events_and_says_so(self):
        status, resp, data = self.srv.request("GET", "/api/task?id=tparent")
        self.assertEqual(status, 200)
        self.assertLess(len(data), server_mod.MAX_BODY)
        d = json.loads(data)
        self.assertTrue(d["events_truncated"])
        self.assertEqual(d["events_total"], 502)  # add, claim and 500 notes
        self.assertLess(len(d["events"]), d["events_total"])
        self.assertEqual(d["events"][-1]["id"], 508)  # newest kept, oldest first
        self.assertFalse(d["events_complete"])

    def test_small_task_is_not_marked_truncated(self):
        d = json.loads(self.srv.request("GET", "/api/task?id=tchild1")[2])
        self.assertFalse(d["events_truncated"])
        self.assertEqual(d["events_total"], len(d["events"]))


class ModelSizeTests(unittest.TestCase):
    def rows(self, n):
        return [event(i, "t", "a", "note", i, "x" * 100) for i in range(1, n + 1)]

    def test_newest_that_fit_by_count(self):
        kept, cut = model.newest_that_fit(self.rows(10), 3, 10**9)
        self.assertEqual([r["id"] for r in kept], [8, 9, 10])
        self.assertTrue(cut)

    def test_newest_that_fit_by_budget_keeps_at_least_one(self):
        kept, cut = model.newest_that_fit(self.rows(10), 100, 1)
        self.assertEqual([r["id"] for r in kept], [10])
        self.assertTrue(cut)

    def test_nothing_cut_when_it_fits(self):
        kept, cut = model.newest_that_fit(self.rows(4), 100, 10**9)
        self.assertEqual((len(kept), cut), (4, False))

    def test_cost_counts_escaped_non_bmp_characters(self):
        base = model.json_cost(event(1, "t", "a", "note", 1, ""))
        self.assertEqual(model.json_cost(event(1, "t", "a", "note", 1, "\U0001F600")) - base, 12)

    def test_feed_after_cuts_at_the_budget_and_stops_the_cursor_before_the_cut(self):
        evs = self.rows(10)
        cost = model.json_cost(dict(evs[0], task_title=None))
        rows, cursor = model.feed_after(evs, {}, 0, 100, None, None, None, cost * 3 + 1)
        self.assertEqual([r["id"] for r in rows], [3, 2, 1])  # newest first
        self.assertEqual(cursor, 3)  # event 4 is the first left out; ask again from 3
        rows2, cursor2 = model.feed_after(evs, {}, cursor, 100, None, None, None, 10**9)
        self.assertEqual([r["id"] for r in rows2][0], 10)
        self.assertEqual(cursor2, 10)

    def test_feed_after_with_a_filter_skips_non_matching_events_safely(self):
        evs = self.rows(6)
        for e in evs[::2]:
            e["author"] = "other"  # ids 1, 3, 5
        rows, cursor = model.feed_after(evs, {}, 0, 100, "a", None, None, 10**9)
        self.assertEqual([r["id"] for r in rows], [6, 4, 2])
        self.assertEqual(cursor, 6)

    def test_feed_after_with_the_limit_cuts_like_the_budget(self):
        rows, cursor = model.feed_after(self.rows(10), {}, 2, 3, None, None, None, 10**9)
        self.assertEqual([r["id"] for r in rows], [5, 4, 3])
        self.assertEqual(cursor, 5)

    def test_feed_after_always_returns_one_row_even_if_it_is_over_budget(self):
        rows, cursor = model.feed_after(self.rows(3), {}, 0, 100, None, None, None, 1)
        self.assertEqual([r["id"] for r in rows], [1])
        self.assertEqual(cursor, 1)

    def test_feed_after_on_an_empty_board(self):
        self.assertEqual(model.feed_after([], {}, 7, 10, None, None, None, 100), ([], 7))

    def test_feed_page_info_reports_a_budget_cut(self):
        info = {}
        evs = self.rows(10)
        cost = model.json_cost(dict(evs[0], task_title=None))
        rows = model.feed_page(evs, {}, None, None, 100, None, None, None, cost * 2 + 10, info)
        self.assertEqual(len(rows), 2)
        self.assertTrue(info["cut"])
        info2 = {}
        model.feed_page(evs, {}, None, None, 100, None, None, None, 10**9, info2)
        self.assertFalse(info2["cut"])

    def test_task_view_caps_children(self):
        tasks = [task("p")] + [task("c%d" % i, parent="p", seq=i) for i in range(model.TASK_CHILD_LIMIT + 30)]
        v = model.task_view("p", tasks, [], [], 0)
        self.assertEqual(len(v["children"]), model.TASK_CHILD_LIMIT)
        self.assertEqual(v["children_total"], model.TASK_CHILD_LIMIT + 30)


# ---------------------------------------------------------------- 8. truncation is reported


class TruncationReportTests(unittest.TestCase):
    def test_decisions_window_flag(self):
        from console import service as svc
        tasks, events, decisions = small_board()
        old = svc.DECISION_WINDOW
        svc.DECISION_WINDOW = 1  # the one decision fills the window: it may have been cut
        try:
            s = BoardService(FakeSource(tasks, events, decisions), ttl=0.0)
            meta = s.snapshot()
            self.assertTrue(s.meta(meta)["truncated"]["decisions"])
            card = s.scorecard_payload((None, None), None, None)
        finally:
            svc.DECISION_WINDOW = old
        dec = {m["id"]: m for m in card["overall"]["metrics"]}["decisions_per_done"]
        self.assertIn("decisions are only the newest window", dec["caveat"])

    def test_flag_is_off_when_the_window_is_not_full(self):
        s = BoardService(FakeSource(*small_board()), ttl=0.0)
        self.assertFalse(s.meta(s.snapshot())["truncated"]["decisions"])
        self.assertFalse(s.meta(s.snapshot())["truncated"]["events"])

    def test_event_truncation_reaches_the_scorecard_and_the_compare_view(self):
        from console import service as svc
        old = svc.EVENT_WINDOW
        svc.EVENT_WINDOW = 3
        try:
            s = BoardService(FakeSource(*small_board()), ttl=0.0)
            card = s.scorecard_payload((None, None), (0, 1), None)
        finally:
            svc.EVENT_WINDOW = old
        ms = {m["id"]: m for m in card["overall"]["metrics"]}
        self.assertTrue(ms["time_to_done"]["truncated"])
        self.assertIn("newest window", card["compare"]["warning"])
        self.assertTrue(card["meta"]["truncated"]["events"])

    def test_api_exposes_the_decision_flag(self):
        srv = Running(FakeSource(*small_board()))
        try:
            d = json.loads(srv.request("GET", "/api/now")[2])
            self.assertIn("decisions", d["meta"]["truncated"])
        finally:
            srv.stop()


if __name__ == "__main__":
    unittest.main()
