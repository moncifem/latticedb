"""Write mode over HTTP: every gate a request has to pass, each proven with a request that fails it.

Reading the file top to bottom is the order the server checks: Host, the switch, the route, Origin, Content-Type,
Content-Length, the token, the body, the decision. Every test that expects a refusal also asserts that the helper
was never asked (FakeRunner.calls is empty): a refusal that still wrote would be the bug.
"""

import json
import threading
import time
import unittest

import helpers  # noqa: F401
from actions_helpers import ACTOR, REQ, TOKEN, FakeRunner, Write, board

from console import actions
from console.server import MAX_POST
from test_server import EXPECTED_CSP, Running, board_source


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


class Case(unittest.TestCase):
    def setUp(self):
        self.w = Write()
        self.addCleanup(self.w.stop)
        self.runner = self.w.runner

    def assert_no_write(self):
        self.assertEqual(self.runner.calls, [])


# ---------------------------------------------------------------- 1. the switch


class WritesOff(unittest.TestCase):
    """Started without --allow-writes: no Actions object exists, so there is nothing for a request to reach."""

    def setUp(self):
        self.srv = Running(board_source())
        self.addCleanup(self.srv.stop)

    def test_every_post_is_a_403(self):
        good = {"Origin": "http://127.0.0.1:%d" % self.srv.port, "Content-Type": "application/json",
                actions.TOKEN_HEADER: TOKEN}
        for path in ("/api/act", "/api/act/bulk", "/api/now", "/", "/api/unknown", "/static/app.js", "/api/act?x=1"):
            for headers in (good, {}, {"Origin": "null"}):
                status, resp, data = self.srv.request("POST", path, headers=headers, body=b"{}")
                self.assertEqual(status, 403, (path, headers))
                self.assertIn(b"writes are off", data)

    def test_even_a_perfect_request_changes_nothing_and_says_how_to_turn_writes_on(self):
        body = json.dumps({"action": "cancel", "task": "tparent", "request_id": REQ}).encode()
        status, resp, data = self.srv.request("POST", "/api/act", headers={
            "Origin": "http://127.0.0.1:%d" % self.srv.port, "Content-Type": "application/json", actions.TOKEN_HEADER: TOKEN},
            body=body)
        self.assertEqual(status, 403)
        self.assertIn(b"--allow-writes", data)

    def test_the_meta_says_writes_are_off(self):
        _, d, _ = self.srv.get_json("/api/now")
        self.assertEqual(d["meta"]["writes"], {"enabled": False, "actor": None})

    def test_the_inbox_is_readable_and_read_only(self):
        status, d, _ = self.srv.get_json("/api/inbox")
        self.assertEqual(status, 200)
        self.assertEqual(d["meta"]["writes"]["enabled"], False)


# ---------------------------------------------------------------- 2. Host (DNS rebinding)


class HostCheck(Case):
    REBOUND = ["evil.example", "evil.example:%(p)d", "127.0.0.1.evil.example:%(p)d", "localhost.evil.example:%(p)d",
               "127.0.0.1:%(q)d", "127.0.0.1", "localhost", "[::1]:%(p)d", "0.0.0.0:%(p)d", "", "127.0.0.1:%(p)d.evil.example",
               "attacker.example:80"]

    def hosts(self):
        return [h % {"p": self.w.port, "q": self.w.port + 1} for h in self.REBOUND]

    def test_a_post_with_a_foreign_host_is_misdirected_even_with_a_perfect_token_and_origin(self):
        for host in self.hosts():
            status, d, _ = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": REQ}, host=host)
            self.assertEqual(status, 421, host)
        self.assert_no_write()

    def test_the_bulk_route_checks_the_host_too(self):
        for host in self.hosts():
            status, _, _ = self.w.post("/api/act/bulk", {"action": "cancel", "ids": ["topen"], "dry_run": True}, host=host)
            self.assertEqual(status, 421, host)

    def test_the_host_is_checked_before_anything_else_is_said(self):
        # A rebound page must not learn that writes exist, that its token is wrong or that its Origin is wrong.
        status, resp, data = self.w.request("POST", "/api/act", headers={"Origin": "null"}, host="evil.example", body=b"x")
        self.assertEqual(status, 421)
        self.assertNotIn(b"token", data)
        self.assertNotIn(b"Origin", data)

    def test_a_post_with_a_foreign_host_is_misdirected_when_writes_are_off_too(self):
        srv = Running(board_source())
        self.addCleanup(srv.stop)
        self.assertEqual(srv.request("POST", "/api/act", host="evil.example", body=b"{}")[0], 421)
        self.assertEqual(srv.request("POST", "/api/act", body=b"{}")[0], 403)

    def test_reads_are_checked_the_same_way(self):
        for path in ("/api/inbox", "/api/now", "/", "/static/app.js"):
            for host in self.hosts():
                self.assertEqual(self.w.request("GET", path, host=host)[0], 421, (path, host))

    def test_the_two_own_names_and_any_letter_case_are_accepted(self):
        for host in ("127.0.0.1:%d" % self.w.port, "localhost:%d" % self.w.port, "LOCALHOST:%d" % self.w.port):
            self.assertEqual(self.w.request("GET", "/api/inbox", host=host)[0], 200, host)


# ---------------------------------------------------------------- 3. route, 4. Origin


class Route(Case):
    def test_post_goes_to_two_routes_only(self):
        valid = json.dumps({"action": "cancel", "task": "topen", "request_id": REQ}).encode()
        for path in ("/api/now", "/api/inbox", "/", "/api/act/", "/api/act/x", "/api/ACT", "/api/actx", "/api/act%2Fbulk"):
            status, _, data = self.w.request("POST", path, headers=self.w.good_headers(), body=valid)
            self.assertEqual(status, 405, path)
        self.assert_no_write()

    def test_the_405_names_the_methods_that_work(self):
        _, resp, _ = self.w.request("POST", "/api/now", headers=self.w.good_headers(), body=b"{}")
        self.assertEqual(resp.getheader("Allow"), "GET, HEAD")

    def test_a_decision_takes_no_query_string(self):
        body = json.dumps({"action": "cancel", "task": "topen", "request_id": REQ}).encode()  # a valid decision
        status, _, _ = self.w.request("POST", "/api/act?x=1", headers=self.w.good_headers(), body=body)
        self.assertEqual(status, 400)
        self.assert_no_write()

    def test_absolute_and_backslash_targets_are_refused(self):
        for target in ("http://127.0.0.1:%d/api/act" % self.w.port, "\\api\\act"):
            conn_status, _, _ = self.w.request("POST", target, headers=self.w.good_headers(), body=b"{}")
            self.assertEqual(conn_status, 400, target)
        self.assert_no_write()

    def test_other_methods_stay_refused(self):
        for method in ("PUT", "DELETE", "PATCH", "OPTIONS"):
            status, resp, _ = self.w.request(method, "/api/act", headers=self.w.good_headers(), body=b"{}")
            self.assertEqual(status, 405, method)
            self.assertIsNone(resp.getheader("Access-Control-Allow-Origin"))
        self.assert_no_write()


class GetNeverWrites(Case):
    def test_no_get_or_head_reaches_the_helper(self):
        q = "?action=cancel&task=topen&request_id=%s&token=%s" % (REQ, TOKEN)
        for method in ("GET", "HEAD"):
            for path in ("/api/act", "/api/act/bulk", "/api/inbox", "/api/now", "/api/queue", "/api/feed", "/"):
                for suffix in ("", q):
                    self.w.request(method, path + suffix, headers=self.w.good_headers())
        self.assert_no_write()

    def test_the_write_routes_are_not_readable(self):
        for path in ("/api/act", "/api/act/bulk"):
            self.assertEqual(self.w.request("GET", path)[0], 404)

    def test_reading_the_inbox_does_not_change_it(self):
        _, a, _ = self.w.get_json("/api/inbox")
        _, b, _ = self.w.get_json("/api/inbox")
        self.assertEqual([i["id"] for i in a["items"]], [i["id"] for i in b["items"]])
        self.assert_no_write()


class OriginCheck(Case):
    def post_with(self, origin):
        h = self.w.good_headers()
        h["Origin"] = origin
        return self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": REQ}, headers=h)

    def test_a_missing_origin_is_refused(self):
        status, d, _ = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": REQ}, headers={"Origin": None})
        self.assertEqual(status, 403)
        self.assert_no_write()

    def test_null_and_foreign_and_near_miss_origins_are_refused(self):
        p = self.w.port
        for origin in ("null", "", "http://evil.example", "http://evil.example:%d" % p, "https://127.0.0.1:%d" % p,
                       "http://127.0.0.1", "http://127.0.0.1:%d/" % p, "http://127.0.0.1:%d" % (p + 1),
                       "HTTP://127.0.0.1:%d" % p, "http://127.0.0.1:%d.evil.example" % p, "http://[::1]:%d" % p,
                       "file://", "http://localhost.evil.example:%d" % p):
            status, d, _ = self.post_with(origin)
            self.assertEqual(status, 403, origin)
        self.assert_no_write()

    def test_the_two_own_origins_pass(self):
        for origin in ("http://127.0.0.1:%d" % self.w.port, "http://localhost:%d" % self.w.port):
            status, d, _ = self.post_with(origin)
            self.assertEqual(status, 200, origin)

    def test_a_refused_origin_does_not_spend_a_token_attempt(self):
        for _ in range(8):
            self.post_with("http://evil.example")
        self.assertEqual(self.post_with("http://127.0.0.1:%d" % self.w.port)[0], 200)


# ---------------------------------------------------------------- 5. Content-Type, 6. length


class Framing(Case):
    def test_only_json_is_accepted(self):
        for ctype in ("text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/jsonx",
                      "application/json; boundary=x", "application/json; charset=latin-1", "text/json", "", "json"):
            status, _, _ = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": REQ},
                                       headers={"Content-Type": ctype})
            self.assertEqual(status, 415, ctype)
        status, _, _ = self.w.post("/api/act", {}, headers={"Content-Type": None})
        self.assertEqual(status, 415)
        self.assert_no_write()

    def test_json_with_the_utf8_charset_is_accepted(self):
        for ctype in ("application/json", "application/json; charset=utf-8", "application/json;charset=UTF-8", "Application/JSON"):
            status, _, _ = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": "req-" + str(abs(hash(ctype)))[:10]},
                                       headers={"Content-Type": ctype})
            self.assertEqual(status, 200, ctype)

    def test_a_length_is_required_and_must_be_plain_digits(self):
        base = ("POST /api/act HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nOrigin: %s\r\nContent-Type: application/json\r\n%s: %s\r\n"
                % (self.w.port, self.w.origin, actions.TOKEN_HEADER, TOKEN))
        for line in ("", "Content-Length: -1\r\n", "Content-Length: 1e3\r\n", "Content-Length: \xef\xbc\x91\xef\xbc\x92\r\n",
                     "Content-Length: 0x10\r\n", "Content-Length: 1234567\r\n", "Content-Length: 12 34\r\n"):
            raw = (base + line + "\r\n{}").encode("latin-1")
            out = self.w.raw(raw)
            self.assertRegex(out.split(b"\r\n")[0].decode(), r"HTTP/1\.[01] 41[13]|HTTP/1\.[01] 400", line)
        self.assert_no_write()

    def test_chunked_bodies_are_refused(self):
        raw = ("POST /api/act HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nOrigin: %s\r\nContent-Type: application/json\r\n%s: %s\r\n"
               "Transfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n" % (self.w.port, self.w.origin, actions.TOKEN_HEADER, TOKEN)).encode()
        self.assertIn(b" 411 ", self.w.raw(raw).split(b"\r\n")[0] + b" ")
        self.assert_no_write()

    def test_chunked_with_a_length_is_refused_too(self):
        # both headers at once is how a request gets smuggled past a proxy; this server takes neither
        body = json.dumps({"action": "cancel", "task": "topen", "request_id": REQ})
        raw = ("POST /api/act HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nOrigin: %s\r\nContent-Type: application/json\r\n%s: %s\r\n"
               "Transfer-Encoding: chunked\r\nContent-Length: %d\r\n\r\n%s" % (
                   self.w.port, self.w.origin, actions.TOKEN_HEADER, TOKEN, len(body), body)).encode()
        self.assertIn(b" 411 ", self.w.raw(raw).split(b"\r\n")[0] + b" ")
        self.assert_no_write()

    def test_a_large_body_is_refused_before_it_is_read(self):
        status, d, _ = self.w.post("/api/act", None, raw=b"x" * (MAX_POST + 1))
        self.assertEqual(status, 413)
        self.assert_no_write()

    def test_a_body_that_ends_early_is_a_bad_request_not_a_hang(self):
        raw = ("POST /api/act HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nOrigin: %s\r\nContent-Type: application/json\r\n%s: %s\r\n"
               "Content-Length: 100\r\n\r\n{\"a\":" % (self.w.port, self.w.origin, actions.TOKEN_HEADER, TOKEN)).encode()
        import socket
        s = socket.create_connection(("127.0.0.1", self.w.port), timeout=10)
        s.sendall(raw)
        s.shutdown(socket.SHUT_WR)
        out = s.recv(65536)
        s.close()
        self.assertIn(b" 400 ", out.split(b"\r\n")[0] + b" ")
        self.assert_no_write()


# ---------------------------------------------------------------- 7. the token


class Token(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.w = Write(clock=self.clock)
        self.addCleanup(self.w.stop)

    def attempt(self, token):
        return self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": REQ},
                           headers={actions.TOKEN_HEADER: token})

    def test_a_missing_or_wrong_token_is_a_401(self):
        self.assertEqual(self.attempt(None)[0], 401)
        self.assertEqual(self.attempt("nope")[0], 401)
        self.assertEqual(self.attempt(TOKEN[:-1])[0], 401)
        self.assertEqual(self.w.runner.calls, [])

    def test_the_token_is_not_accepted_anywhere_else(self):
        body = json.dumps({"action": "cancel", "task": "topen", "request_id": REQ, "token": TOKEN}).encode()
        h = self.w.good_headers(None)
        del h[actions.TOKEN_HEADER]
        for where in ({"Authorization": TOKEN}, {"Cookie": "token=" + TOKEN}, {"X-Token": TOKEN}):
            h2 = dict(h, **where)
            self.assertEqual(self.w.request("POST", "/api/act", headers=h2, body=body)[0], 401)
        self.assertEqual(self.w.request("POST", "/api/act?token=" + TOKEN, headers=h, body=body)[0], 400)
        self.assertEqual(self.w.runner.calls, [])

    def test_five_wrong_tokens_lock_writes_and_the_right_one_waits(self):
        for _ in range(5):
            self.assertEqual(self.attempt("wrong")[0], 401)
        status, d, resp = self.attempt(TOKEN)
        self.assertEqual(status, 429)
        self.assertEqual(resp.getheader("Retry-After"), "60")
        self.assertEqual(self.w.runner.calls, [])
        self.clock.t = 61.0
        self.assertEqual(self.attempt(TOKEN)[0], 200)

    def test_the_decision_rate_is_bounded(self):
        for i in range(60):
            status, _, _ = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": "req-%08d" % i})
            self.assertEqual(status, 200)
        status, _, resp = self.w.post("/api/act", {"action": "cancel", "task": "topen", "request_id": "req-99999999"})
        self.assertEqual(status, 429)
        self.assertEqual(len(self.w.runner.calls), 60)

    def test_the_token_is_never_echoed_by_any_response(self):
        seen = []
        sends = [self.attempt(TOKEN), self.attempt("wrong-" + TOKEN), self.attempt(None)]
        for status, d, resp in sends:
            seen.append(json.dumps(d) + str(resp.getheaders()))
        for path in ("/", "/static/app.js", "/api/now", "/api/inbox", "/api/queue", "/api/act", "/nope"):
            status, resp, data = self.w.request("GET", path)
            seen.append(data.decode("utf-8", "replace") + str(resp.getheaders()))
        status, resp, data = self.w.request("POST", "/api/act", headers=self.w.good_headers(), body=b"not json")
        seen.append(data.decode() + str(resp.getheaders()))
        for text in seen:
            self.assertNotIn(TOKEN, text)
            self.assertNotIn(TOKEN[:12], text)


# ---------------------------------------------------------------- 8. the body


class Body(Case):
    def test_malformed_json_is_a_400(self):
        for raw in (b"", b"not json", b"[]", b'{"action": "cancel", "action": "approve", "task": "topen", "request_id": "req-0123456789"}',
                    b'{"a": NaN}', b"\xff\xfe", b'"cancel"', b"null", b"{" * 5000):
            status, d, _ = self.w.post("/api/act", None, raw=raw)
            self.assertEqual(status, 400, raw[:30])
        self.assert_no_write()

    def test_the_action_list_is_closed_over_http(self):
        for action in ("delete", "approve_all", "bulk_approve", "sql", "", None, "APPROVE"):
            status, d, _ = self.w.act(action=action)
            self.assertEqual(status, 400, action)
        self.assert_no_write()

    def test_every_field_is_validated(self):
        for fields in ({"task": "a b"}, {"task": "x" * 65}, {"task": 5}, {"request_id": "short"}, {"request_id": "bad id 0123"},
                       {"comment": "a\\b"}, {"comment": "x" * 1001}, {"extra": 1}, {"sha": "a" * 64}):
            status, d, _ = self.w.act(**fields)
            self.assertEqual(status, 400, fields)
        self.assert_no_write()

    def test_the_errors_are_fixed_sentences_without_the_input_echoed(self):
        status, d, _ = self.w.act(task="a;DROP TABLE tasks")
        self.assertEqual(status, 400)
        self.assertNotIn("DROP", json.dumps(d))

    def test_a_bulk_approve_does_not_exist(self):
        status, d, _ = self.w.post("/api/act/bulk", {"action": "approve", "ids": ["tapprove"], "dry_run": False,
                                                     "request_id": REQ, "confirm": 1})
        self.assertEqual(status, 400)
        self.assert_no_write()


# ---------------------------------------------------------------- 9. the decision


class Decision(Case):
    def test_a_valid_decision_reaches_the_helper_exactly_as_validated(self):
        status, d, resp = self.w.act(action="note", task="tdead", comment="  look   at\nthis ")
        self.assertEqual(status, 200)
        self.assertEqual(self.runner.calls, [["act", "note", "tdead", "--request-id", REQ, "--comment", "look at this"]])
        self.assertEqual(d["event_id"], 901)
        self.assertEqual(d["actor"], ACTOR)

    def test_answers_carry_the_strict_headers_and_no_cors(self):
        status, d, resp = self.w.act()
        self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP)
        self.assertEqual(resp.getheader("Cache-Control"), "no-store")
        self.assertEqual(resp.getheader("X-Content-Type-Options"), "nosniff")
        self.assertTrue(resp.getheader("Content-Type").startswith("application/json"))
        self.assertFalse([k for k, _ in resp.getheaders() if k.lower().startswith("access-control-")])
        for status, d, resp in (self.w.act(task="a b"), self.w.post("/api/act", None, raw=b"x")):
            self.assertEqual(resp.getheader("Content-Security-Policy"), EXPECTED_CSP)

    def test_a_repeat_with_the_same_request_id_is_one_write(self):
        first = self.w.act()
        again = self.w.act()
        self.assertEqual((first[0], again[0]), (200, 200))
        self.assertTrue(again[1]["repeated"])
        self.assertEqual(len(self.runner.calls), 1)

    def test_two_requests_at_once_with_one_request_id_are_one_write(self):
        self.runner.answer = lambda args: (time.sleep(0.2), (0, "done 5\n"))[1]
        out = []
        threads = [threading.Thread(target=lambda: out.append(self.w.act()[0])) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(10)
        self.assertEqual(out, [200] * 4)
        self.assertEqual(len(self.runner.calls), 1)

    def test_a_refusal_by_the_board_is_passed_on_as_its_fixed_sentence(self):
        self.runner.answer = lambda args: (5, "refused self_approval\n")
        status, d, _ = self.w.act(action="approve", task="tmine", sha="a" * 64)
        self.assertEqual(status, 403)
        self.assertEqual((d["ok"], d["code"]), (False, "self_approval"))
        self.assertEqual(d["error"], actions.REFUSALS["self_approval"][1])

    def test_a_stale_approval_is_a_409(self):
        self.runner.answer = lambda args: (5, "refused stale\n")
        status, d, _ = self.w.act(action="approve", task="tapprove", sha="b" * 64)
        self.assertEqual((status, d["code"]), (409, "stale"))

    def test_the_helper_is_asked_with_the_hash_the_page_gave(self):
        sha = "06b004e02f7816e31814914e79b237bc3fa0f5e163ad859376ffc8bdc5dff4e5"
        self.w.act(action="approve", task="tapprove", sha=sha, comment="ok")
        self.assertEqual(self.runner.calls[0], ["act", "approve", "tapprove", "--request-id", REQ, "--hash", sha, "--comment", "ok"])

    def test_a_broken_helper_is_a_502_with_no_detail(self):
        self.runner.answer = lambda args: (127, "sh: board: not found; PGPASSWORD=hunter2")
        status, d, _ = self.w.act()
        self.assertEqual(status, 502)
        self.assertNotIn("hunter2", json.dumps(d))
        self.assertNotIn("not found", json.dumps(d))

    def test_a_runner_that_raises_is_an_internal_error_not_a_traceback(self):
        def boom(args):
            raise RuntimeError("secret internals /home/user/.pgpass")
        self.runner.answer = boom
        status, d, _ = self.w.act()
        self.assertEqual(status, 500)
        self.assertNotIn("pgpass", json.dumps(d))

    def test_the_inbox_shows_the_result_at_once(self):
        # A service that caches for a minute: the decision has to drop the cache, or the page would show the old row.
        w = Write(runner=FakeRunner())
        self.addCleanup(w.stop)
        w.service.ttl = 60.0
        _, before, _ = w.get_json("/api/inbox")
        self.assertIn("tapprove", [i["id"] for i in before["items"]])
        for t in w.source.task_rows:
            if t["id"] == "tapprove":
                t["state"] = "open"  # what the board would now hold
        _, still, _ = w.get_json("/api/inbox")
        self.assertIn("tapprove", [i["id"] for i in still["items"]])  # cached
        w.act(action="cancel", task="topen", request_id="req-after-0001")
        _, after, _ = w.get_json("/api/inbox")
        self.assertNotIn("tapprove", [i["id"] for i in after["items"]])

    def test_the_meta_names_the_person_decisions_are_recorded_for(self):
        _, d, _ = self.w.get_json("/api/now")
        self.assertEqual(d["meta"]["writes"], {"enabled": True, "actor": ACTOR})


class Bulk(Case):
    def test_a_dry_run_over_http(self):
        status, d, _ = self.w.post("/api/act/bulk", {"action": "cancel", "ids": ["topen", "tdead"], "dry_run": True})
        self.assertEqual(status, 200)
        self.assertEqual((d["count"], d["dry_run"]), (1, True))
        self.assert_no_write()

    def test_the_confirmed_run_writes_one_per_task(self):
        status, d, _ = self.w.post("/api/act/bulk", {"action": "cancel", "ids": ["topen", "tclaimed"], "dry_run": False,
                                                     "request_id": REQ, "confirm": 2})
        self.assertEqual((status, d["done"]), (200, 2))
        self.assertEqual(len(self.runner.calls), 2)

    def test_a_wrong_count_writes_nothing(self):
        status, d, _ = self.w.post("/api/act/bulk", {"action": "cancel", "ids": ["topen", "tclaimed"], "dry_run": False,
                                                     "request_id": REQ, "confirm": 5})
        self.assertEqual((status, d["code"]), (409, "count_changed"))
        self.assert_no_write()


if __name__ == "__main__":
    unittest.main()
