import http.client
import json
import socket
import threading
import unittest

import helpers  # noqa: F401
from helpers import FakeSource, event, small_board, task

from console import server as server_mod
from console.demo import DemoSource
from console.server import ConsoleServer, check_bind
from console.service import BoardService
from console.source import ConfigError, SourceError

EXPECTED_CSP = ("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; "
                "img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")


class Running:
    """A real server on a loopback port, served from a thread."""

    def __init__(self, source, **kw):
        self.service = BoardService(source, ttl=0.0, **kw)
        self.server = ConsoleServer(self.service, 0)
        self.port = self.server.port
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self.thread.start()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)

    def request(self, method, path, headers=None, host=None, body=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        h = dict(headers or {})
        conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        conn.putheader("Host", host if host is not None else "127.0.0.1:%d" % self.port)
        for k, v in h.items():
            conn.putheader(k, v)
        if body is not None:
            conn.putheader("Content-Length", str(len(body)))
        conn.endheaders(body)
        resp = conn.getresponse()
        data = resp.read()
        out = (resp.status, resp, data)
        conn.close()
        return out

    def get_json(self, path, **kw):
        status, resp, data = self.request("GET", path, **kw)
        return status, json.loads(data.decode("ascii")), resp


def board_source():
    tasks, events, decisions = small_board()
    return FakeSource(tasks, events, decisions, now=5000)


class ServerCase(unittest.TestCase):
    source_factory = staticmethod(board_source)

    def setUp(self):
        self.src = self.source_factory()
        self.srv = Running(self.src)
        self.addCleanup(self.srv.stop)


class HeaderTests(ServerCase):
    PATHS = ["/", "/static/app.js", "/static/style.css", "/api/now", "/api/feed", "/api/task?id=tparent",
             "/api/scorecard", "/nope", "/api/feed?after=abc", "/api/task?id=missing"]

    def test_every_response_has_the_security_headers(self):
        for path in self.PATHS:
            status, resp, _ = self.srv.request("GET", path)
            self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP, path)
            self.assertEqual(resp.getheader("X-Content-Type-Options"), "nosniff", path)
            self.assertEqual(resp.getheader("Referrer-Policy"), "no-referrer", path)
            self.assertEqual(resp.getheader("Cache-Control"), "no-store", path)

    def test_error_responses_have_them_too(self):
        for method, path, kw in [("POST", "/api/now", {}), ("GET", "/", {"host": "evil.example"}),
                                 ("GET", "/api/zzz", {}), ("DELETE", "/", {})]:
            status, resp, _ = self.srv.request(method, path, **kw)
            self.assertGreaterEqual(status, 400)
            self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP, (method, path))
            self.assertEqual(resp.getheader("X-Content-Type-Options"), "nosniff")

    def test_no_cors_headers_anywhere(self):
        for path in self.PATHS + ["/api/now"]:
            _, resp, _ = self.srv.request("GET", path, headers={"Origin": "http://evil.example"})
            names = [k.lower() for k, _ in resp.getheaders()]
            self.assertFalse([n for n in names if n.startswith("access-control-")], path)

    def test_options_preflight_is_refused(self):
        status, resp, _ = self.srv.request("OPTIONS", "/api/now", headers={
            "Origin": "http://evil.example", "Access-Control-Request-Method": "GET"})
        self.assertEqual(status, 405)
        self.assertIsNone(resp.getheader("Access-Control-Allow-Origin"))

    def test_content_types(self):
        self.assertEqual(self.srv.request("GET", "/")[1].getheader("Content-Type"), "text/html; charset=utf-8")
        self.assertTrue(self.srv.request("GET", "/static/app.js")[1].getheader("Content-Type").startswith("text/javascript"))
        self.assertTrue(self.srv.request("GET", "/static/style.css")[1].getheader("Content-Type").startswith("text/css"))
        self.assertTrue(self.srv.request("GET", "/api/now")[1].getheader("Content-Type").startswith("application/json"))

    def test_csp_blocks_inline_script_and_external_loads(self):
        self.assertNotIn("unsafe-inline", EXPECTED_CSP)
        self.assertNotIn("unsafe-eval", EXPECTED_CSP)
        self.assertNotIn("http", EXPECTED_CSP)

    def test_server_header_does_not_name_the_python_version(self):
        server = self.srv.request("GET", "/")[1].getheader("Server")
        self.assertNotIn("Python", server)


class MethodAndHostTests(ServerCase):
    def test_only_get_and_head(self):
        for method in ("POST", "PUT", "DELETE", "PATCH", "OPTIONS"):
            status, resp, _ = self.srv.request(method, "/api/now", body=b"x=1" if method in ("POST", "PUT", "PATCH") else None)
            self.assertEqual(status, 405, method)
            self.assertEqual(resp.getheader("Allow"), "GET, HEAD")

    def test_head_has_headers_and_no_body(self):
        status, resp, data = self.srv.request("HEAD", "/")
        self.assertEqual(status, 200)
        self.assertEqual(data, b"")
        self.assertGreater(int(resp.getheader("Content-Length")), 100)
        self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP)

    def test_wrong_host_is_421(self):
        for host in ("evil.example", "evil.example:%d" % self.srv.port, "127.0.0.1", "127.0.0.1:1",
                     "0.0.0.0:%d" % self.srv.port, "127.0.0.1.evil.example:%d" % self.srv.port, ""):
            status, _, data = self.srv.request("GET", "/api/now", host=host)
            self.assertEqual(status, 421, host)
            self.assertNotIn(b"tparent", data)

    def test_host_check_applies_to_static_files_too(self):
        self.assertEqual(self.srv.request("GET", "/", host="evil.example")[0], 421)
        self.assertEqual(self.srv.request("GET", "/static/app.js", host="evil.example")[0], 421)

    def test_loopback_names_with_the_right_port_are_accepted(self):
        for host in ("127.0.0.1:%d" % self.srv.port, "localhost:%d" % self.srv.port, "LOCALHOST:%d" % self.srv.port):
            self.assertEqual(self.srv.request("GET", "/api/now", host=host)[0], 200, host)

    def test_missing_host_header_over_http_1_0(self):
        s = socket.create_connection(("127.0.0.1", self.srv.port), timeout=5)
        s.sendall(b"GET /api/now HTTP/1.0\r\n\r\n")
        data = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        s.close()
        self.assertTrue(data.startswith(b"HTTP/1.0 421") or data.startswith(b"HTTP/1.1 421"), data[:40])

    def test_absolute_form_target_is_refused(self):
        status, _, _ = self.srv.request("GET", "http://evil.example/api/now")
        self.assertEqual(status, 400)

    def test_overlong_target(self):
        status, _, _ = self.srv.request("GET", "/api/feed?agent=" + "a" * 3000)
        self.assertEqual(status, 414)

    def test_backslash_target_is_refused(self):
        self.assertEqual(self.srv.request("GET", "/\\evil")[0], 400)


class BindTests(unittest.TestCase):
    def test_only_loopback_is_allowed(self):
        for host in ("0.0.0.0", "", "::", "192.168.1.5", "localhost", "::1", "example.com"):
            with self.assertRaises(ConfigError, msg=host):
                check_bind(host)
        self.assertEqual(check_bind("127.0.0.1"), "127.0.0.1")

    def test_constructor_refuses_other_binds(self):
        with self.assertRaises(ConfigError):
            ConsoleServer(BoardService(FakeSource()), 0, host="0.0.0.0")

    def test_listening_socket_is_on_loopback(self):
        srv = ConsoleServer(BoardService(FakeSource()), 0)
        try:
            self.assertEqual(srv.server_address[0], "127.0.0.1")
            self.assertEqual(srv.url, "http://127.0.0.1:%d/" % srv.port)
        finally:
            srv.server_close()


class QueryValidationTests(ServerCase):
    def status(self, path):
        return self.srv.request("GET", path)[0]

    def test_bad_cursors(self):
        for bad in ("abc", "-1", "1.5", "", "1e3", "99999999999999999999", "9007199254740993", "%201", "0x10", "%31%2e5"):
            self.assertEqual(self.status("/api/feed?after=" + bad), 400, bad)
        for bad in ("abc", "0", "-5"):
            self.assertEqual(self.status("/api/feed?before=" + bad), 400, bad)

    def test_good_cursors(self):
        for good in ("0", "5", "9007199254740992"):
            self.assertEqual(self.status("/api/feed?after=" + good), 200, good)

    def test_limit_cap(self):
        self.assertEqual(self.status("/api/feed?limit=0"), 400)
        self.assertEqual(self.status("/api/feed?limit=501"), 400)
        self.assertEqual(self.status("/api/feed?limit=500"), 200)
        self.assertEqual(self.status("/api/feed?limit=1"), 200)
        self.assertEqual(self.status("/api/feed?limit=ten"), 400)

    def test_unknown_and_repeated_parameters(self):
        self.assertEqual(self.status("/api/feed?sql=1"), 400)
        self.assertEqual(self.status("/api/feed?after=1&after=2"), 400)
        self.assertEqual(self.status("/api/now?x=1"), 400)
        self.assertEqual(self.status("/api/feed?%"), 400)

    def test_filters_are_validated(self):
        for bad in ("a%20b", "a'b", "a;b", "a%00b", "a" * 65, "<x>", "a%27%20OR%201=1"):
            self.assertEqual(self.status("/api/feed?agent=" + bad), 400, bad)
            self.assertEqual(self.status("/api/feed?task=" + bad), 400, bad)
        self.assertEqual(self.status("/api/feed?kind=drop"), 400)
        self.assertEqual(self.status("/api/feed?kind=note"), 200)
        self.assertEqual(self.status("/api/feed?agent=al.ice-1_x"), 200)

    def test_task_id_is_required_and_validated(self):
        self.assertEqual(self.status("/api/task"), 400)
        self.assertEqual(self.status("/api/task?id=a%27b"), 400)
        self.assertEqual(self.status("/api/task?id=" + "a" * 65), 400)
        self.assertEqual(self.status("/api/task?id=nope"), 404)

    def test_scorecard_window_validation(self):
        self.assertEqual(self.status("/api/scorecard?from=10&to=5"), 400)
        self.assertEqual(self.status("/api/scorecard?from=5&to=5"), 400)
        self.assertEqual(self.status("/api/scorecard?from=x"), 400)
        self.assertEqual(self.status("/api/scorecard?from=99999999999"), 400)
        self.assertEqual(self.status("/api/scorecard?from=1&to=2"), 200)
        self.assertEqual(self.status("/api/scorecard?from2=1&to2=0"), 400)

    def test_error_body_is_json_with_a_short_message(self):
        status, body, _ = self.srv.get_json("/api/feed?after=abc")
        self.assertEqual(status, 400)
        self.assertEqual(list(body), ["error"])
        self.assertLess(len(body["error"]), 200)

    def test_database_is_not_queried_for_a_rejected_request(self):
        before = len(self.src.calls)
        self.status("/api/feed?after=abc")
        self.status("/api/zzz")
        self.assertEqual(len(self.src.calls), before)


class ApiTests(ServerCase):
    def test_now(self):
        status, d, _ = self.srv.get_json("/api/now")
        self.assertEqual(status, 200)
        self.assertEqual(d["counts"], {"open": 1, "claimed": 1, "done": 1, "dead": 0, "cancelled": 0,
                                       "awaiting_approval": 0, "other": 0})
        self.assertEqual({a["name"] for a in d["agents"]}, {"alice", "bob", "maint"})
        self.assertFalse(d["meta"]["demo"])
        self.assertIsNone(d["meta"]["banner"])
        self.assertEqual(d["head"], 8)

    def test_feed_newest_first_and_cursor(self):
        _, d, _ = self.srv.get_json("/api/feed?limit=3")
        self.assertEqual([e["id"] for e in d["events"]], [8, 7, 6])
        self.assertEqual(d["cursor"], 8)
        self.assertTrue(d["has_more"])
        self.assertEqual(d["agents"], ["alice", "bob", "maint"])
        _, d2, _ = self.srv.get_json("/api/feed?after=8")
        self.assertEqual(d2["events"], [])
        self.assertEqual(d2["cursor"], 8)
        self.src.event_rows.append(event(9, "tparent", "alice", "note", 2300, "later"))
        self.src.event_rows.append(event(10, "tparent", "alice", "note", 2400, "latest"))
        _, d3, _ = self.srv.get_json("/api/feed?after=8")
        self.assertEqual([e["id"] for e in d3["events"]], [10, 9])
        self.assertEqual(d3["cursor"], 10)

    def test_cursor_beyond_head_asks_the_client_to_reset(self):
        _, d, _ = self.srv.get_json("/api/feed?after=500")
        self.assertTrue(d["reset"])
        self.assertEqual(d["cursor"], 8)

    def test_feed_filters(self):
        _, d, _ = self.srv.get_json("/api/feed?agent=bob&kind=note")
        self.assertEqual([e["id"] for e in d["events"]], [6])
        _, d, _ = self.srv.get_json("/api/feed?task=tchild2")
        self.assertEqual([e["id"] for e in d["events"]], [3])

    def test_feed_before_pages_back(self):
        _, d, _ = self.srv.get_json("/api/feed?before=4")
        self.assertEqual([e["id"] for e in d["events"]], [3, 2, 1])

    def test_task(self):
        status, d, _ = self.srv.get_json("/api/task?id=tchild1")
        self.assertEqual(status, 200)
        self.assertEqual([p["id"] for p in d["parents"]], ["tparent"])
        self.assertEqual([e["id"] for e in d["events"]], [2, 5, 6, 7, 8])
        self.assertEqual(len(d["decisions"]), 1)

    def test_scorecard_shape(self):
        status, d, _ = self.srv.get_json("/api/scorecard")
        self.assertEqual(status, 200)
        ids = [m["id"] for m in d["overall"]["metrics"]]
        self.assertIn("completion_rate", ids)
        self.assertEqual([m["id"] for m in d["not_measurable"]], ["rework_rate", "spot_check_agreement"])
        self.assertIsNone(d["compare"])
        for m in d["overall"]["metrics"]:
            self.assertEqual(m["status"], "insufficient")  # three tasks are far below every minimum

    def test_scorecard_compare_is_returned_when_asked(self):
        _, d, _ = self.srv.get_json("/api/scorecard?from=1000&to=1300&from2=0&to2=1000")
        self.assertIsNotNone(d["compare"])
        self.assertEqual({r["status"] for r in d["compare"]["rows"]}, {"insufficient"})

    def test_non_ascii_text_is_escaped_in_json(self):
        self.src.event_rows.append(event(9, "tparent", "alice", "note", 2300, "café ☃ <b>"))
        status, resp, data = self.srv.request("GET", "/api/feed?limit=1")
        self.assertTrue(data.isascii())
        self.assertEqual(json.loads(data)["events"][0]["body"], "café ☃ <b>")

    def test_hostile_text_travels_as_json_data_only(self):
        hostile = "</script><img src=x onerror=alert(1)>"
        self.src.event_rows.append(event(9, "tparent", "alice", "note", 2300, hostile))
        status, resp, data = self.srv.request("GET", "/api/feed?limit=1")
        self.assertTrue(resp.getheader("Content-Type").startswith("application/json"))
        self.assertEqual(json.loads(data)["events"][0]["body"], hostile)


class SizeCapTests(ServerCase):
    def test_long_text_is_clipped(self):
        self.src.event_rows.append(event(9, "tparent", "alice", "note", 2300, "x" * 20000))
        _, d, _ = self.srv.get_json("/api/feed?limit=1")
        self.assertEqual(len(d["events"][0]["body"]), 4096)

    def test_oversized_answer_becomes_a_500_not_a_huge_download(self):
        old = server_mod.MAX_BODY
        server_mod.MAX_BODY = 500
        try:
            status, resp, data = self.srv.request("GET", "/api/now")
        finally:
            server_mod.MAX_BODY = old
        self.assertEqual(status, 500)
        self.assertLess(len(data), 200)
        self.assertIn(b"too large", data)

    def test_feed_page_size_is_bounded_by_the_limit_cap(self):
        for i in range(9, 700):
            self.src.event_rows.append(event(i, "tparent", "alice", "note", 3000 + i, "n"))
        _, d, _ = self.srv.get_json("/api/feed?limit=500")
        self.assertEqual(len(d["events"]), 500)


class ErrorHandlingTests(ServerCase):
    def test_source_error_is_a_502_with_credentials_scrubbed(self):
        self.src.error = SourceError("the database query failed: postgresql://dev:topsecret@h/x password=topsecret")
        status, body, _ = self.srv.get_json("/api/now")
        self.assertEqual(status, 502)
        self.assertNotIn("topsecret", json.dumps(body))

    def test_unexpected_exception_is_a_500_without_details(self):
        self.src.error = RuntimeError("secret internal detail /home/user/x")
        status, resp, data = self.srv.request("GET", "/api/now")
        self.assertEqual(status, 500)
        self.assertNotIn(b"secret", data)
        self.assertNotIn(b"/home", data)

    def test_static_pages_do_not_need_the_database(self):
        self.src.error = SourceError("down")
        self.assertEqual(self.srv.request("GET", "/")[0], 200)
        self.assertEqual(self.srv.request("GET", "/static/app.js")[0], 200)


class DemoServerTests(unittest.TestCase):
    def setUp(self):
        self.srv = Running(DemoSource(reveal_every=0))
        self.addCleanup(self.srv.stop)

    def test_every_api_answer_carries_the_demo_banner_text(self):
        for path in ("/api/now", "/api/feed", "/api/scorecard", "/api/task?id=" + self.first_task()):
            status, d, _ = self.srv.get_json(path)
            self.assertEqual(status, 200, path)
            self.assertTrue(d["meta"]["demo"], path)
            self.assertEqual(d["meta"]["banner"], "DEMO DATA, not your board", path)

    def first_task(self):
        return self.srv.get_json("/api/feed?limit=1")[1]["events"][0]["task_id"]

    def test_page_has_a_banner_element_and_the_script_shows_it(self):
        html = self.srv.request("GET", "/")[2].decode()
        self.assertIn('id="banner"', html)
        self.assertIn("DEMO DATA, not your board", html)
        js = self.srv.request("GET", "/static/app.js")[2].decode()
        self.assertIn("meta.banner", js)
        self.assertIn("applyMeta(", js)

    def test_demo_scorecard_has_numbers_somewhere(self):
        _, d, _ = self.srv.get_json("/api/scorecard")
        self.assertTrue([m for m in d["overall"]["metrics"] if m["status"] == "ok"])

    def test_demo_shows_attention_items(self):
        _, d, _ = self.srv.get_json("/api/now")
        a = d["attention"]
        self.assertTrue(a["stale"] or a["open_too_long"] or a["double_claims"])

    def test_the_real_source_flag_is_off_for_a_non_demo_server(self):
        other = Running(board_source())
        try:
            self.assertFalse(other.get_json("/api/now")[1]["meta"]["demo"])
        finally:
            other.stop()


class QueueApiTests(ServerCase):
    source_factory = staticmethod(lambda: FakeSource(
        [task("run1"), task("blk", seq=2), task("held", "claimed", "bob", seq=3),
         task("gone", "dead", seq=4)],
        [], [], now=9000,
        leases=[helpers.lease("blk", blocked="gone"), helpers.lease("held", until=8900, hb=8800),
                helpers.lease("gone", attempts=3, last_error="boom")]))

    def test_queue_answer_shape(self):
        status, d, resp = self.srv.get_json("/api/queue")
        self.assertEqual(status, 200)
        self.assertEqual(d["totals"], {"runnable": 1, "blocked": 1, "claimed": 1, "retrying": 0,
                                       "dead": 1, "awaiting": 0})
        self.assertEqual(d["claimed"][0]["expired"], True)
        self.assertEqual(d["blocked"][0]["forever"], True)
        self.assertEqual(d["dead"][0]["last_error"], "boom")
        self.assertEqual(d["now"], 9000)
        self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP)

    def test_queue_takes_no_parameters_and_only_get(self):
        self.assertEqual(self.srv.request("GET", "/api/queue?x=1")[0], 400)
        self.assertEqual(self.srv.request("POST", "/api/queue", body=b"")[0], 405)
        self.assertEqual(self.srv.request("GET", "/api/queue", host="evil.example")[0], 421)

    def test_now_carries_the_attention_items_and_queue_totals(self):
        _, d, _ = self.srv.get_json("/api/now")
        self.assertEqual(len(d["attention"]["expired_leases"]), 1)
        self.assertEqual(len(d["attention"]["dead_tasks"]), 1)
        self.assertEqual(len(d["attention"]["blocked_forever"]), 1)
        self.assertEqual(d["queue_totals"]["runnable"], 1)

    def test_the_new_event_kinds_are_valid_feed_filters(self):
        for kind in ("fail", "expired", "release", "approve", "cancel", "block", "unblock", "fenced"):
            self.assertEqual(self.srv.request("GET", "/api/feed?kind=" + kind)[0], 200, kind)
        self.assertEqual(self.srv.request("GET", "/api/feed?kind=bogus")[0], 400)

    def test_demo_queue_has_the_banner(self):
        demo_srv = Running(DemoSource(reveal_every=0))
        try:
            _, d, _ = demo_srv.get_json("/api/queue")
            self.assertEqual(d["meta"]["banner"], "DEMO DATA, not your board")
            self.assertGreaterEqual(d["totals"]["dead"], 1)
        finally:
            demo_srv.stop()

    def test_the_queue_answer_has_no_html_in_text_fields(self):
        src = FakeSource([task("a", "dead", title="<img src=x onerror=alert(1)>")], [], [], now=9000,
                         leases=[helpers.lease("a", last_error="<script>alert(1)</script>")])
        srv = Running(src)
        try:
            status, resp, data = srv.request("GET", "/api/queue")
            self.assertEqual(status, 200)
            self.assertTrue(data.isascii())  # ASCII-escaped JSON; the text is data, not markup
            self.assertEqual(resp.getheader("Content-Type"), "application/json; charset=utf-8")
        finally:
            srv.stop()


if __name__ == "__main__":
    unittest.main()
