"""board-mcp speaks the protocol the way docs/mcp-v0.md says the gateway does.

Run: python3 -m unittest discover -s integrations/board/tests -p "test_mcp_*.py"
"""

import json
import os
import signal
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from mcpbase import Fixture, schema_problems  # noqa: E402
from mcpclient import LEGACY, MODERN, META, RpcFailure  # noqa: E402

META_V = "io.modelcontextprotocol/protocolVersion"
META_C = "io.modelcontextprotocol/clientCapabilities"


def line(**kw):
    kw.setdefault("jsonrpc", "2.0")
    return json.dumps(kw)


class Modern(Fixture):
    def test_server_discover_names_the_modern_revision_only_before_any_handshake(self):
        c = self.start()
        r = c.request("server/discover")["result"]
        self.assertEqual(r["resultType"], "complete")
        self.assertEqual(r["supportedVersions"], [MODERN])
        self.assertIn("tools", r["capabilities"])
        self.assertEqual(r["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "board-mcp")

    def test_a_request_without_meta_is_invalid_params(self):
        c = self.start()
        m = c.request("tools/list", meta=False)
        self.assertEqual(m["error"]["code"], -32602)
        self.assertEqual(m["error"]["data"]["field"], "_meta")

    def test_a_meta_without_the_version_or_the_capabilities_is_invalid_params(self):
        c = self.start()
        for meta, field in (({META_C: {}}, META_V), ({META_V: MODERN}, META_C)):
            rid = c.send_request("ping", {"_meta": meta}, meta=False)
            m = c.wait_for(rid)
            self.assertEqual(m["error"]["code"], -32602, meta)
            self.assertEqual(m["error"]["data"]["field"], field)

    def test_meta_must_be_an_object(self):
        c = self.start()
        for bad in ("x", None, [], 3):
            rid = c.send_request("ping", {"_meta": bad}, meta=False)
            self.assertEqual(c.wait_for(rid)["error"]["code"], -32602, bad)

    def test_an_unsupported_version_is_32022_with_the_supported_list(self):
        c = self.start()
        rid = c.send_request("ping", {"_meta": {META_V: "1900-01-01", META_C: {}}}, meta=False)
        e = c.wait_for(rid)["error"]
        self.assertEqual(e["code"], -32022)
        self.assertEqual(e["data"]["supported"], [MODERN])
        self.assertEqual(e["data"]["requested"], "1900-01-01")

    def test_the_legacy_version_is_refused_until_initialize(self):
        c = self.start()
        rid = c.send_request("ping", {"_meta": {META_V: LEGACY, META_C: {}}}, meta=False)
        self.assertEqual(c.wait_for(rid)["error"]["code"], -32022)

    def test_a_non_string_or_odd_version_is_echoed_nowhere(self):
        c = self.start()
        for v in (7, "a b\"c", "x" * 400):
            rid = c.send_request("ping", {"_meta": {META_V: v, META_C: {}}}, meta=False)
            e = c.wait_for(rid)["error"]
            self.assertEqual(e["code"], -32022)
            self.assertNotIn("requested", e["data"])

    def test_ping_is_an_empty_complete_result(self):
        c = self.start()
        self.assertEqual(c.request("ping")["result"], {"resultType": "complete"})

    def test_unknown_methods_are_32601_even_without_meta(self):
        c = self.start()
        for method in ("resources/list", "prompts/list", "logging/setLevel", "tools/delete", "x"):
            m = c.request(method, meta=False)
            self.assertEqual(m["error"]["code"], -32601, method)

    def test_initialize_is_not_needed_for_the_modern_flow(self):
        c = self.start()
        names = [t["name"] for t in c.request("tools/list")["result"]["tools"]]
        self.assertIn("board_next", names)


class Legacy(Fixture):
    def test_initialize_switches_to_the_legacy_era(self):
        c = self.start()
        m = c.request("initialize", {"protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}})
        r = m["result"]
        self.assertEqual(r["protocolVersion"], LEGACY)
        self.assertEqual(r["resultType"], "complete")
        self.assertEqual(r["capabilities"], {"tools": {"listChanged": False}})
        self.assertEqual(r["serverInfo"]["name"], "board-mcp")
        self.assertIn("untrusted", r["instructions"])

    def test_after_initialize_meta_is_optional_and_both_versions_are_accepted(self):
        c = self.start(legacy=True)
        self.assertEqual(c.request("ping")["result"], {"resultType": "complete"})
        d = c.request("server/discover")["result"]
        self.assertEqual(d["supportedVersions"], [MODERN, LEGACY])
        rid = c.send_request("ping", {"_meta": {META_V: LEGACY}}, meta=False)
        self.assertEqual(c.wait_for(rid)["result"], {"resultType": "complete"})

    def test_after_initialize_a_present_meta_is_still_checked(self):
        c = self.start(legacy=True)
        rid = c.send_request("ping", {"_meta": {META_V: "1900-01-01"}}, meta=False)
        e = c.wait_for(rid)["error"]
        self.assertEqual(e["code"], -32022)
        self.assertEqual(e["data"]["supported"], [MODERN, LEGACY])
        rid = c.send_request("ping", {"_meta": "x"}, meta=False)
        self.assertEqual(c.wait_for(rid)["error"]["code"], -32602)

    def test_initialize_needs_its_three_members(self):
        c = self.start()
        for params in ({}, {"protocolVersion": LEGACY}, {"protocolVersion": LEGACY, "capabilities": {}},
                       {"protocolVersion": 1, "capabilities": {}, "clientInfo": {}}):
            m = c.request("initialize", params, meta=False)
            self.assertEqual(m["error"]["code"], -32602, params)
        # a failed initialize did not switch the mode: meta is still required
        self.assertEqual(c.request("ping", meta=False)["error"]["code"], -32602)

    def test_a_missing_meta_never_selects_legacy(self):
        c = self.start()
        c.request("ping", meta=False)
        self.assertEqual(c.request("tools/list", meta=False)["error"]["code"], -32602)

    def test_notifications_are_never_answered(self):
        c = self.start(legacy=True)
        c.notify("notifications/initialized")
        c.notify("notifications/unknown", {"a": 1})
        c.notify("ping")
        c.notify("tools/call", {"name": "board_list"})
        c.notify("no/such")
        c.request("ping")
        self.assertEqual(len(c.lines), 2, c.lines)  # the initialize answer and the ping answer


class Framing(Fixture):
    def test_garbage_is_a_parse_error_with_a_null_id(self):
        c = self.start()
        c.send_raw("{not json\n")
        c.send_raw(b"\xff\xfe\n")
        for _ in range(2):
            pass
        c.request("ping")
        errs = [json.loads(x) for x in c.lines if "error" in x]
        self.assertEqual([e["error"]["code"] for e in errs], [-32700, -32700])
        self.assertTrue(all(e["id"] is None for e in errs))

    def test_duplicate_keys_and_non_json_constants_are_parse_errors(self):
        c = self.start()
        c.send_raw('{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}\n')
        c.send_raw('{"jsonrpc":"2.0","id":1,"method":"ping","params":{"x":NaN}}\n')
        c.request("ping")
        errs = [json.loads(x)["error"]["code"] for x in c.lines if "error" in x]
        self.assertEqual(errs, [-32700, -32700])

    def test_batches_and_non_objects_are_invalid_requests(self):
        c = self.start()
        c.send_raw('[{"jsonrpc":"2.0","id":1,"method":"ping"}]\n')
        c.send_raw("42\n")
        c.send_raw('"x"\n')
        c.request("ping")
        errs = [json.loads(x)["error"] for x in c.lines if "error" in x]
        self.assertEqual([e["code"] for e in errs], [-32600, -32600, -32600])
        self.assertIn("batch", errs[0]["message"])

    def test_the_jsonrpc_members_are_strict(self):
        c = self.start()
        bad = [line(id=1, method="ping", extra=1), '{"id":2,"method":"ping"}', line(id=3, method=7),
               line(id=True, method="ping"), line(id=None, method="ping"), line(id=1.5, method="ping"),
               line(id=4, method="ping", params=[1]), line(id=5, result={})]
        for b in bad:
            c.send_raw(b + "\n")
        c.request("ping", rid=999)
        errs = [json.loads(x) for x in c.lines if "error" in x]
        self.assertEqual(len(errs), len(bad))
        self.assertTrue(all(e["error"]["code"] == -32600 for e in errs), errs)

    def test_string_and_integer_ids_are_echoed_exactly(self):
        c = self.start()
        for rid in ("abc", 0, 7, -3, "1"):
            self.assertEqual(c.request("ping", rid=rid)["id"], rid)

    def test_deeply_nested_or_huge_messages_are_refused(self):
        c = self.start()
        deep = "[" * 40 + "]" * 40
        c.send_raw('{"jsonrpc":"2.0","id":1,"method":"ping","params":{"a":%s}}\n' % deep)
        many = ",".join("1" for _ in range(5000))
        c.send_raw('{"jsonrpc":"2.0","id":2,"method":"ping","params":{"a":[%s]}}\n' % many)
        c.request("ping")
        errs = [json.loads(x) for x in c.lines if "error" in x]
        self.assertEqual([e["error"]["code"] for e in errs], [-32600, -32600])

    def test_a_line_over_one_mebibyte_is_refused_and_skipped_and_the_server_goes_on(self):
        c = self.start()
        c.send_raw(b'{"jsonrpc":"2.0","id":1,"method":"ping","params":{"x":"' + b"a" * (1024 * 1024 + 10) + b'"}}\n')
        m = c.request("ping")
        self.assertEqual(m["result"], {"resultType": "complete"})
        first = json.loads(c.lines[0])
        self.assertEqual(first["error"]["code"], -32600)
        self.assertIn("too large", first["error"]["message"])
        self.assertIsNone(first["id"])

    def test_crlf_and_blank_lines_are_tolerated(self):
        c = self.start()
        c.send_raw("\n\r\n   \n")
        c.send_raw(line(id=1, method="ping", params={"_meta": META}) + "\r\n")
        self.assertEqual(c.wait_for(1)["result"], {"resultType": "complete"})

    def test_stdout_holds_only_json_messages_and_logs_go_to_stderr(self):
        c = self.start()
        c.request("ping")
        c.call("board_list")
        c.send_raw("garbage\n")
        c.request("ping")
        for ln in c.lines:
            self.assertIsInstance(json.loads(ln), dict)
            self.assertNotIn("\n", ln)
        c.close()
        time.sleep(0.1)
        self.assertTrue(any(e.startswith("board-mcp: started") for e in c.err))

    def test_output_is_pure_ascii_json_so_no_line_separator_can_split_it(self):
        c = self.start()
        c.call("board_add", {"title": "cr\u00e8me \u2603 \U0001f600"})
        r = c.call("board_list")
        for ln in c.lines:
            ln.encode("ascii")
        self.assertIn("cr\u00e8me", r["structuredContent"]["tasks"][0]["untrusted"]["title"])

    def test_closing_stdin_ends_the_server_promptly_with_status_zero(self):
        c = self.start()
        c.request("ping")
        t0 = time.monotonic()
        self.assertEqual(c.close(wait=10), 0)
        self.assertLess(time.monotonic() - t0, 5)

    def test_sigterm_ends_the_server(self):
        c = self.start()
        c.request("ping")
        c.proc.send_signal(signal.SIGTERM)
        self.assertEqual(c.proc.wait(timeout=10), 143)


class ToolsCall(Fixture):
    def test_an_unknown_tool_is_32602_not_a_result(self):
        c = self.start()
        with self.assertRaises(RpcFailure) as cm:
            c.call("board_cancel")
        self.assertEqual(cm.exception.error["code"], -32602)

    def test_tools_call_parameters_are_checked(self):
        c = self.start()
        for params in ({}, {"name": 5}, {"name": "board_list", "arguments": [1]}, {"name": "board_list", "x": 1}):
            m = c.request("tools/call", params)
            self.assertEqual(m["error"]["code"], -32602, params)

    def test_bad_arguments_are_a_tool_error_with_issue_named_not_a_protocol_error(self):
        c = self.start()
        for name, args in (("board_list", {"limit": 0}), ("board_list", {"limit": "5"}), ("board_list", {"state": "bogus"}),
                           ("board_list", {"nope": 1}), ("board_show", {}), ("board_show", {"task_id": "a b"}),
                           ("board_note", {"lease_id": "ls_x", "text": "hi"}), ("board_add", {"title": ""}),
                           ("board_add", {"title": "x", "blocked_by": ["a"] * 33}),
                           ("board_next", {"lease_seconds": 5}), ("board_queue", {"limit": 99})):
            r = c.call(name, args)
            self.assertTrue(r["isError"], (name, args))
            self.assertEqual(r["structuredContent"]["error"]["code"], "invalid_input")
            self.assertEqual(r["structuredContent"]["ok"], False)
            self.assertEqual(r["structuredContent"]["error"]["exit_code"], 0)

    def test_an_invalid_input_message_never_echoes_the_value(self):
        c = self.start()
        r = c.call("board_show", {"task_id": "SENTINEL VALUE !!"})
        self.assertNotIn("SENTINEL", self.text(r))
        r = c.call("board_list", {"SENTINELKEY": 1})
        self.assertTrue(r["isError"])

    def test_an_integer_valued_float_is_accepted_and_a_fraction_is_not(self):
        c = self.start()
        c.send_raw(line(id=50, method="tools/call", params={"_meta": META, "name": "board_list", "arguments": {"limit": 5.0}}) + "\n")
        self.assertFalse(c.wait_for(50)["result"]["isError"])
        c.send_raw(line(id=51, method="tools/call", params={"_meta": META, "name": "board_list", "arguments": {"limit": 5.5}}) + "\n")
        self.assertTrue(c.wait_for(51)["result"]["isError"])

    def test_a_result_has_a_text_block_and_structured_content_that_agree_on_ok(self):
        c = self.start()
        r = c.call("board_list")
        self.assertEqual(r["resultType"], "complete")
        self.assertEqual(r["content"][0]["type"], "text")
        self.assertIs(r["isError"], False)
        self.assertIs(r["structuredContent"]["ok"], True)

    def test_a_duplicate_request_id_in_flight_is_refused(self):
        self.conf(delay={"list": 2})
        c = self.start()
        c.send_raw(line(id=9, method="tools/call", params={"_meta": META, "name": "board_list", "arguments": {}}) + "\n")
        time.sleep(0.4)
        c.send_raw(line(id=9, method="tools/call", params={"_meta": META, "name": "board_list", "arguments": {}}) + "\n")
        deadline = time.time() + 5
        while time.time() < deadline and not any('"error"' in x for x in c.lines):
            time.sleep(0.05)
        errs = [json.loads(x) for x in c.lines if '"error"' in x]
        self.assertEqual(errs[0]["error"]["code"], -32600)
        self.assertIn("already in progress", errs[0]["error"]["message"])

    def text(self, r):
        return r["content"][0]["text"]


class Concurrency(Fixture):
    def test_requests_overlap_and_each_gets_its_own_answer(self):
        self.conf(delay={"list": 1.0})
        c = self.start()
        t0 = time.monotonic()
        ids = [c.send_request("tools/call", {"name": "board_list", "arguments": {}}) for _ in range(4)]
        for rid in ids:
            c.wait_for(rid)
        self.assertLess(time.monotonic() - t0, 3.0)  # four one-second calls ran together, not one after another

    def test_at_most_n_helpers_run_at_once(self):
        self.conf(delay={"list": 1.0})
        c = self.start(BOARD_MCP_HELPERS="2")
        t0 = time.monotonic()
        ids = [c.send_request("tools/call", {"name": "board_list", "arguments": {}}) for _ in range(4)]
        for rid in ids:
            c.wait_for(rid)
        self.assertGreater(time.monotonic() - t0, 1.8)  # two at a time: two rounds

    def test_a_cancelled_request_kills_the_helper_and_is_never_answered(self):
        self.conf(delay={"list": 30})
        c = self.start()
        rid = c.send_request("tools/call", {"name": "board_list", "arguments": {}})
        deadline = time.time() + 5
        while time.time() < deadline and not self.calls():
            time.sleep(0.05)
        pid = self.calls()[0]["pid"]
        c.notify("notifications/cancelled", {"requestId": rid, "reason": "user"})
        deadline = time.time() + 5
        while time.time() < deadline:
            try:
                os.kill(pid, 0)
            except OSError:
                break
            time.sleep(0.05)
        else:
            self.fail("the helper process survived the cancellation")
        self.conf()
        self.assertEqual(c.request("ping")["result"], {"resultType": "complete"})
        self.assertFalse([x for x in c.lines if json.loads(x).get("id") == rid], c.lines)

    def test_a_cancel_naming_an_odd_request_id_does_not_kill_the_server(self):
        c = self.start()
        for rid in ([], {}, [1, 2], {"a": 1}, True, None, 1.5, "x" * 300):
            c.notify("notifications/cancelled", {"requestId": rid})
        c.notify("notifications/cancelled", {"requestId": []})
        self.assertEqual(c.request("ping")["result"], {"resultType": "complete"})
        self.assertIsNone(c.proc.poll())
        self.assertFalse([e for e in c.err if "Traceback" in e or "internal error" in e], c.err)

    def test_a_string_request_id_is_capped_at_256_characters(self):
        c = self.start()
        ok = "i" * 256
        self.assertEqual(c.request("ping", rid=ok)["id"], ok)
        c.send_raw(json.dumps({"jsonrpc": "2.0", "id": "j" * 257, "method": "ping"}) + "\n")
        c.send_raw(json.dumps({"jsonrpc": "2.0", "id": "k" * 900000, "method": "nope"}) + "\n")
        c.request("ping")
        errs = [json.loads(x) for x in c.lines if '"error"' in x]
        self.assertEqual([e["error"]["code"] for e in errs], [-32600, -32600])
        self.assertTrue(all(e["id"] is None and len(json.dumps(e)) < 500 for e in errs))

    def test_cancelling_an_unknown_request_is_ignored(self):
        c = self.start()
        c.notify("notifications/cancelled", {"requestId": 12345})
        c.notify("notifications/cancelled", {})
        self.assertEqual(c.request("ping")["result"], {"resultType": "complete"})

    def test_a_call_that_takes_too_long_is_killed_and_reported_with_an_unknown_outcome(self):
        self.conf(delay={"list": 30})
        c = self.start(BOARD_MCP_TIMEOUT="1")
        t0 = time.monotonic()
        r = c.call("board_list")
        self.assertLess(time.monotonic() - t0, 8)
        self.assertTrue(r["isError"])
        self.assertEqual(r["structuredContent"]["error"]["code"], "timeout")
        self.assertEqual(r["structuredContent"]["error"]["exit_code"], 124)
        deadline = time.time() + 5
        pid = self.calls()[0]["pid"]
        while time.time() < deadline:
            try:
                os.kill(pid, 0)
            except OSError:
                return
            time.sleep(0.05)
        self.fail("the helper survived the timeout")

    def text(self, r):
        return r["content"][0]["text"]


if __name__ == "__main__":
    unittest.main()
