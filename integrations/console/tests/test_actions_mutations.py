"""Planted faults in write mode: every one must make its named tests fail.

For each fault this copies the console package with its tests, replaces one piece of text of one file that must
occur exactly once, and runs the named tests inside the copy. The fault is caught when that run fails. A fault
nobody catches means a rule of write mode (the gate, the token, the Origin and Host checks, the request id, the
bulk rules, what the page may send) is not pinned by a test. The faults of the board helper's own rules
(self-approval, the stale hash, the comment, the request id in the database) are in
integrations/board/tests/test_act_mutations.py.

Run: python3 -m unittest discover -s integrations/console/tests -p "test_actions_mutations.py"
"""

import concurrent.futures
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
CONSOLE_DIR = os.path.dirname(HERE)

U = "test_actions."
S = "test_actions_server."
J = "test_actions_static."
M = "test_actions_demo."
I = "test_inbox."

# name: (file under integrations/console, text that occurs once, replacement, tests that must catch it)
MUTATIONS = {
    # ---- the gate
    "the_token_is_compared_with_equals": (
        "console/actions.py", "if not hmac.compare_digest(raw, self._token):", "if raw != self._token:",
        [U + "Gate.test_the_comparison_is_constant_time"]),
    "wrong_tokens_never_lock": (
        "console/actions.py", "if len(self._bad) >= self.MAX_BAD:", "if False:",
        [U + "Gate.test_five_wrong_tokens_lock_everything_for_a_minute", S + "Token.test_five_wrong_tokens_lock_writes_and_the_right_one_waits"]),
    "the_right_token_works_while_locked": (
        "console/actions.py", "            if now < self._locked_until:\n                return \"locked\"\n", "",
        [U + "Gate.test_five_wrong_tokens_lock_everything_for_a_minute"]),
    "a_good_token_does_not_clear_the_count": (
        "console/actions.py", "            self._bad = []\n            self._ok = ", "            self._ok = ",
        [U + "Gate.test_a_good_token_clears_the_count"]),
    "the_decision_rate_is_unbounded": (
        "console/actions.py", "if len(self._ok) >= self.MAX_WRITES:", "if False:",
        [U + "Gate.test_sixty_decisions_a_minute_and_no_more", S + "Token.test_the_decision_rate_is_bounded"]),
    "a_missing_token_passes": (
        "console/server.py", "            if verdict is not None:", "            if False:",
        [S + "Token.test_a_missing_or_wrong_token_is_a_401"]),
    "the_token_is_taken_from_another_header": (
        "console/server.py", "actions.gate.check(self.headers.get(actions_mod.TOKEN_HEADER))",
        "actions.gate.check(self.headers.get(actions_mod.TOKEN_HEADER) or self.headers.get(\"Authorization\"))",
        [S + "Token.test_the_token_is_not_accepted_anywhere_else"]),
    "the_token_is_printed_in_the_url_line": (
        "console/__main__.py", 'print("console: serving %s from %s, %s" % (server.url, where, mode), flush=True)',
        'print("console: serving %s?t=%s from %s, %s" % (server.url, token, where, mode), flush=True)',
        [M + "CommandLine.test_the_token_is_not_in_the_url_line", M + "CommandLine.test_the_flag_turns_on_a_gate_and_prints_the_token_once"]),
    "the_token_is_printed_twice": (
        "console/__main__.py", '        print("console: write token (shown once, paste it into the page): %s" % token, flush=True)',
        '        print("console: write token (shown once, paste it into the page): %s" % token, flush=True)\n        print("console: (again) %s" % token, flush=True)',
        [M + "CommandLine.test_the_flag_turns_on_a_gate_and_prints_the_token_once"]),
    "the_token_is_the_same_every_start": (
        "console/actions.py", "    return secrets.token_urlsafe(32)", "    return \"fixed-token-for-every-run-0123456789\"",
        [M + "CommandLine.test_every_start_makes_a_new_token", U + "Gate.test_new_tokens_are_long_and_different"]),
    "writes_are_on_without_the_flag": (
        "console/__main__.py", "        if args.allow_writes:\n            actor = ", "        if True:\n            actor = ",
        [M + "CommandLine.test_writes_are_off_by_default"]),
    "post_works_with_writes_off": (
        "console/server.py", "            if actions is None:\n                self._error(403,", "            if False:\n                self._error(403,",
        [S + "WritesOff.test_every_post_is_a_403"]),
    # ---- Host and Origin
    "the_host_is_not_checked_on_post": (
        "console/server.py",
        '            host = (self.headers.get("Host") or "").strip().lower()\n            if host not in allowed_hosts_ref():\n'
        '                self._error(421, "misdirected request: unexpected Host header")\n                return\n            if actions is None:',
        "            if actions is None:",
        [S + "HostCheck.test_a_post_with_a_foreign_host_is_misdirected_even_with_a_perfect_token_and_origin",
         S + "HostCheck.test_the_bulk_route_checks_the_host_too"]),
    "the_host_is_checked_after_the_switch": (
        "console/server.py",
        '            host = (self.headers.get("Host") or "").strip().lower()\n            if host not in allowed_hosts_ref():\n'
        '                self._error(421, "misdirected request: unexpected Host header")\n                return\n            if actions is None:\n'
        '                self._error(403, "writes are off: this console was not started with --allow-writes")\n                return\n',
        '            if actions is None:\n                self._error(403, "writes are off: this console was not started with --allow-writes")\n                return\n'
        '            host = (self.headers.get("Host") or "").strip().lower()\n            if host not in allowed_hosts_ref():\n'
        '                self._error(421, "misdirected request: unexpected Host header")\n                return\n',
        [S + "HostCheck.test_a_post_with_a_foreign_host_is_misdirected_when_writes_are_off_too"]),
    "the_host_is_not_checked_on_reads": (
        "console/server.py", '            if host not in allowed_hosts_ref():\n                self._error(421, "misdirected request: unexpected Host header")\n                return\n            if len(self.path) > MAX_PATH:',
        "            if len(self.path) > MAX_PATH:", [S + "HostCheck.test_reads_are_checked_the_same_way"]),
    "origin_is_not_checked": (
        "console/server.py", "if origin is None or origin not in (allowed_origins_ref() if allowed_origins_ref else ()):", "if False:",
        [S + "OriginCheck.test_null_and_foreign_and_near_miss_origins_are_refused", S + "OriginCheck.test_a_missing_origin_is_refused"]),
    "a_missing_origin_is_accepted": (
        "console/server.py", "if origin is None or origin not in (allowed_origins_ref() if allowed_origins_ref else ()):",
        "if origin is not None and origin not in (allowed_origins_ref() if allowed_origins_ref else ()):",
        [S + "OriginCheck.test_a_missing_origin_is_refused"]),
    "origin_is_matched_by_prefix": (
        "console/server.py", "if origin is None or origin not in (allowed_origins_ref() if allowed_origins_ref else ()):",
        "if origin is None or not any(origin.startswith(o) for o in (allowed_origins_ref() if allowed_origins_ref else ())):",
        [S + "OriginCheck.test_null_and_foreign_and_near_miss_origins_are_refused"]),
    "any_localhost_port_is_an_origin": (
        "console/server.py", '        self._origins = {"http://127.0.0.1:%d" % self.port, "http://localhost:%d" % self.port}',
        '        self._origins = {"http://127.0.0.1:%d" % self.port, "http://localhost:%d" % self.port, "http://127.0.0.1", "null"}',
        [S + "OriginCheck.test_null_and_foreign_and_near_miss_origins_are_refused"]),
    # ---- the request
    "get_can_write": (
        "console/server.py", '                elif path == "/api/inbox":',
        '                elif path == "/api/act":\n                    self._json(*actions.act({"action": "cancel", "task": "topen", "request_id": "req-0123456789"}))\n'
        '                elif path == "/api/inbox":',
        [S + "GetNeverWrites.test_the_write_routes_are_not_readable", S + "GetNeverWrites.test_no_get_or_head_reaches_the_helper"]),
    "the_content_type_is_not_checked": (
        "console/server.py", '            if ctype not in ("application/json", "application/json;charset=utf-8"):', "            if False:",
        [S + "Framing.test_only_json_is_accepted"]),
    "a_chunked_body_is_accepted": (
        "console/server.py", '            if self.headers.get("Transfer-Encoding") is not None:', "            if False:",
        [S + "Framing.test_chunked_bodies_are_refused", S + "Framing.test_chunked_with_a_length_is_refused_too"]),
    "the_body_size_is_not_capped": (
        "console/server.py", "            if n > MAX_POST:", "            if False:", [S + "Framing.test_a_large_body_is_refused_before_it_is_read"]),
    "the_length_is_not_plain_digits": (
        "console/server.py", "if length_header is None or not length_header.isascii() or not length_header.isdigit() or len(length_header) > 6:", "if length_header is None:",
        [S + "Framing.test_a_length_is_required_and_must_be_plain_digits"]),
    "a_duplicate_key_is_accepted": (
        "console/actions.py", "            if k in d:", "            if False:", [U + "StrictJson.test_refuses_what_could_smuggle_a_second_meaning",
                                                                          S + "Body.test_malformed_json_is_a_400"]),
    "a_query_string_is_accepted_on_post": (
        "console/server.py", "            if parts.query:\n                self._error(400,", "            if False:\n                self._error(400,",
        [S + "Route.test_a_decision_takes_no_query_string"]),
    "post_goes_to_any_route": (
        "console/server.py", "            if parts.path not in POST_ROUTES:\n                self._error(405,", "            if False:\n                self._error(405,",
        [S + "Route.test_post_goes_to_two_routes_only"]),
    # ---- the decision
    "the_action_list_is_open": (
        "console/actions.py", "    if action not in ACTIONS:  # unknown or missing: the list is closed", "    if False:",
        [U + "ParseAct.test_the_action_list_is_closed", S + "Body.test_the_action_list_is_closed_over_http"]),
    "unknown_fields_are_ignored": (
        "console/actions.py", "    if extra:\n        raise BadAction(\"unknown field %r for %s\"", "    if False:\n        raise BadAction(\"unknown field %r for %s\"",
        [U + "ParseAct.test_unknown_fields_are_refused_per_action"]),
    "task_ids_are_matched_loosely": (
        "console/actions.py", '_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,64}")', '_ID_RE = re.compile(r"[A-Za-z0-9_\\n -]{1,64}")',
        [U + "ParseAct.test_task_ids_are_matched_in_full"]),
    "an_approval_needs_no_hash_here": (
        "console/actions.py", '        out["sha"] = _text(body, "sha", _SHA_RE, "the 64 hex digits of the content hash shown with the task")',
        '        out["sha"] = body.get("sha")', [U + "ParseAct.test_an_approval_needs_the_64_hex_digits_that_were_shown"]),
    "a_rejection_needs_no_comment_here": (
        "console/actions.py", 'out["comment"] = clean_comment(body.get("comment"), action in ("reject", "note"))',
        'out["comment"] = clean_comment(body.get("comment"), False)', [U + "ParseAct.test_reject_and_note_need_a_comment"]),
    "comments_keep_their_control_characters": (
        "console/actions.py", '    if "\\\\" in text or _CONTROL.search(text):', "    if False:", [U + "ParseAct.test_comments_are_one_clean_line"]),
    "a_bulk_approve_exists": (
        "console/actions.py", 'BULK_ACTIONS = ("cancel", "requeue")', 'BULK_ACTIONS = ("cancel", "requeue", "approve")',
        [U + "ParseBulk.test_the_bulk_list_is_cancel_and_requeue_and_never_approve", U + "Bulk.test_there_is_no_bulk_approve"]),
    "a_bulk_run_needs_no_confirmation_count": (
        "console/actions.py", '        if req["confirm"] != len(eligible):', "        if False:",
        [U + "Bulk.test_the_real_run_needs_the_count_the_dry_run_reported", S + "Bulk.test_a_wrong_count_writes_nothing"]),
    "a_dry_run_writes": (
        "console/actions.py", '        if req["dry_run"]:\n            return 200, {"ok": True, "dry_run": True,', '        if False:\n            return 200, {"ok": True, "dry_run": True,',
        [U + "Bulk.test_a_dry_run_counts_what_would_change_and_writes_nothing", S + "Bulk.test_a_dry_run_over_http"]),
    "bulk_includes_tasks_it_does_not_apply_to": (
        "console/actions.py", '            elif t["state"] not in APPLIES[action]:', "            elif False:",
        [U + "Bulk.test_a_dry_run_counts_what_would_change_and_writes_nothing"]),
    "bulk_items_share_one_request_id": (
        "console/actions.py", '                rid = hashlib.sha256(("%s:%s:%s" % (req["request_id"], req["action"], item["id"])).encode()).hexdigest()[:40]',
        '                rid = hashlib.sha256(("%s:%s" % (req["request_id"], req["action"])).encode()).hexdigest()[:40]',
        [U + "Bulk.test_the_real_run_writes_one_decision_per_task_with_its_own_request_id"]),
    "a_request_id_is_not_remembered": (
        "console/actions.py", "        prior = self._seen.get(request_id)", "        prior = None",
        [U + "Decisions.test_the_same_request_id_runs_the_helper_once", S + "Decision.test_a_repeat_with_the_same_request_id_is_one_write"]),
    "a_request_id_may_change_its_decision": (
        "console/actions.py", "            if prior[0] != fp:", "            if False:",
        [U + "Decisions.test_the_same_request_id_for_another_decision_is_refused_without_the_helper"]),
    "the_memory_has_no_bound": (
        "console/actions.py", "        while len(self._seen) > SEEN_MAX:", "        while False:", [U + "Decisions.test_the_memory_is_bounded"]),
    "a_refusal_is_remembered": (
        "console/actions.py", "        if res[0] == \"refused\":\n            status, text = REFUSALS.get(res[1], FAILED)\n",
        "        if res[0] == \"refused\":\n            self._seen[request_id] = (fp, {\"ok\": True})\n            status, text = REFUSALS.get(res[1], FAILED)\n",
        [U + "Decisions.test_a_refusal_is_not_remembered_so_the_person_can_try_again"]),
    "two_requests_run_at_once": (
        "console/actions.py", "        with self._lock:\n            return self._one(req, req[\"request_id\"])", "        return self._one(req, req[\"request_id\"])",
        [S + "Decision.test_two_requests_at_once_with_one_request_id_are_one_write"]),
    "helper_text_is_passed_to_the_browser": (
        "console/actions.py", '            return FAILED[0], {"ok": False, "code": "failed", "error": FAILED[1]}',
        '            return FAILED[0], {"ok": False, "code": "failed", "error": out}', [U + "Decisions.test_helper_output_is_never_passed_on"]),
    "the_helper_error_is_echoed_on_a_crash": (
        "console/server.py", '            except Exception:  # never echo internals to the browser\n                self._error(500, "internal error")\n                return\n            self._json(status, answer)',
        '            except Exception as e:\n                self._error(500, str(e))\n                return\n            self._json(status, answer)',
        [S + "Decision.test_a_runner_that_raises_is_an_internal_error_not_a_traceback"]),
    "any_exit_status_is_a_success": (
        "console/actions.py", "    if rc == 0 and m:\n        return (\"ok\"", "    if m:\n        return (\"ok\"", [U + "HelperLanguage.test_anything_else_is_a_failure"]),
    "the_page_cache_survives_a_decision": (
        "console/service.py", "            self._last_refresh = None\n            self._failure = None", "            self._failure = None",
        [S + "Decision.test_the_inbox_shows_the_result_at_once"]),
    "the_helper_runs_as_the_agent_env_author": (
        "console/actions.py", '        self.env["BOARD_AUTHOR"] = actor', '        self.env.setdefault("BOARD_AUTHOR", actor)',
        [U + "Runner.test_it_passes_the_arguments_untouched_and_runs_as_the_person"]),
    "the_activity_switch_reaches_the_helper": (
        "console/actions.py", '        self.env.pop("BOARD_ACTIVITY", None)', "        pass",
        [U + "Runner.test_activity_and_strict_fence_settings_do_not_reach_the_helper"]),
    # ---- what the inbox shows
    "the_hash_is_of_the_title_only": (
        "console/actions.py", '    return hashlib.sha256((title + "\\n" + body).encode("utf-8")).hexdigest()',
        '    return hashlib.sha256(title.encode("utf-8")).hexdigest()', [U + "Hash.test_known_vectors", I + "Contents.test_an_item_carries_the_text_the_person_must_read_and_the_hash_of_it"]),
    "the_inbox_lists_every_task": (
        "console/inbox.py", '    for r in queue["awaiting"]:\n', '    for r in queue["awaiting"] + queue["blocked"]:\n',
        [I + "Contents.test_only_what_needs_a_person_is_listed", I + "Kinds.test_blocked_for_ever_is_listed_and_blocked_briefly_is_not"]),
    "dead_letters_come_before_approvals": (
        "console/inbox.py", 'KINDS = ("approval", "dead", "quiet", "blocked")', 'KINDS = ("dead", "approval", "quiet", "blocked")',
        [I + "Contents.test_approvals_come_first_oldest_first_then_dead_letters"]),
    "the_you_produced_hint_never_fires": (
        "console/inbox.py", '"you_produced": bool(mine & authors),', '"you_produced": False,',
        [I + "Contents.test_with_an_actor_the_task_made_under_the_same_plain_name_is_flagged"]),
    # ---- the demo
    "the_demo_lets_the_creator_approve": (
        "console/demo.py", '            if {t["created_by"], t.get("owner") or ""} & {actor, plain} or plain in wrote:', "            if False:",
        [M + "Demo.test_the_person_who_made_the_task_cannot_approve_or_reject_it"]),
    "the_demo_ignores_a_stale_hash": (
        "console/demo.py", '        if action == "approve" and opts.get("--hash") != actions_mod.task_sha(t["title"], t["body"]):', "        if False:",
        [M + "Demo.test_refusals_use_the_helpers_codes"]),
    "the_demo_forgets_request_ids": (
        "console/demo.py", "        prior = self.user_requests.get(rid)", "        prior = None",
        [M + "Demo.test_the_same_request_id_is_a_noop_that_names_the_first_event"]),
    # ---- the page
    "the_page_offers_a_bulk_approve": (
        "console/static/app.js", 'click: function () { bulkDialog("cancel"); }', 'click: function () { bulkDialog("approve"); }',
        [J + "Requests.test_there_is_no_bulk_approve"]),
    "the_token_goes_into_the_address": (
        "console/static/app.js", '      var r = await fetch(path + (qs ? "?" + qs : ""), init);',
        '      var r = await fetch(path + (qs ? "?" + qs : "") + (opts && opts.body ? "?t=" + writeToken : ""), init);',
        [J + "Token.test_the_token_is_never_stored_or_put_in_an_address"]),
    "the_token_is_remembered_in_storage": (
        "console/static/app.js", "      writeToken = v;\n", "      writeToken = v;\n      sessionStorage.setItem(\"t\", v);\n",
        [J + "Token.test_the_token_is_never_stored_or_put_in_an_address", "test_static.ShippedFilesTests.test_app_js_has_no_dangerous_construct"]),
    "a_refused_token_is_kept": (
        "console/static/app.js", '      if (e.status === 401) {\n        writeToken = "";', "      if (e.status === 401) {\n        writeToken = writeToken;",
        [J + "Token.test_a_refused_token_is_forgotten"]),
    "the_page_sends_no_hash_with_an_approval": (
        "console/static/app.js", '        if (action === "approve") body.sha = item.sha;\n', "", [J + "Decisions.test_approve_sends_the_hash_the_card_showed"]),
    "the_page_lets_you_approve_what_you_made": (
        "console/static/app.js", "var blocked = (a === \"approve\" || a === \"reject\") && item.you_produced;", "var blocked = false;",
        [J + "Decisions.test_approve_and_reject_are_disabled_for_the_task_you_produced"]),
    "the_page_sends_the_person": (
        "console/static/app.js", "        var body = { action: action, task: item.id, request_id: rid };", "        var body = { action: action, task: item.id, request_id: rid, actor: \"human.admin\" };",
        [J + "Requests.test_the_actor_is_never_sent_by_the_page"]),
    "a_dialog_reuses_no_request_id": (
        "console/static/app.js", "        var body = { action: action, task: item.id, request_id: rid };", "        var body = { action: action, task: item.id, request_id: newRequestId() };",
        [J + "Requests.test_one_request_id_per_dialog_so_a_second_press_repeats_instead_of_doubling", J + "Requests.test_every_decision_carries_a_request_id_made_by_the_browser_crypto"]),
    "request_ids_come_from_math_random": (
        "console/static/app.js", "    crypto.getRandomValues(a);", "    for (var q = 0; q < a.length; q++) a[q] = Math.floor(Math.random() * 256);",
        [J + "Requests.test_every_decision_carries_a_request_id_made_by_the_browser_crypto"]),
    "the_bulk_run_skips_the_dry_run": (
        "console/static/app.js", "dry_run: true", "dry_run: false", [J + "Requests.test_bulk_runs_a_dry_run_first_and_confirms_with_its_count"]),
    "the_header_still_says_read_only": (
        "console/static/index.html", 'id="mode-tag">alpha, read-only<', 'id="mode-tag">alpha<', [J + "Page.test_the_header_says_read_only_until_the_server_says_otherwise"]),
    "the_write_panel_starts_visible": (
        "console/static/index.html", '<section id="write-panel" class="write-panel" aria-label="Write mode" hidden></section>',
        '<section id="write-panel" class="write-panel" aria-label="Write mode"></section>', [J + "Page.test_the_write_panel_and_toast_start_hidden"]),
    "the_csp_allows_inline_script": (
        "console/server.py", "script-src 'self';", "script-src 'self' 'unsafe-inline';", [J + "Page.test_the_content_security_policy_is_not_loosened"]),
}


def run_tests(tests, cwd):
    return subprocess.run([sys.executable, "-m", "unittest", *tests], cwd=cwd, env=dict(os.environ), capture_output=True, text=True,
                          timeout=600)


class Mutations(unittest.TestCase):
    def prepare(self, name):
        rel, old, new, _ = MUTATIONS[name]
        with open(os.path.join(CONSOLE_DIR, rel), encoding="utf-8") as f:
            text = f.read()
        self.assertEqual(text.count(old), 1, "the text to mutate must occur exactly once in %s: %r" % (name, old))
        d = tempfile.mkdtemp(prefix="actions-mut-")
        self.addCleanup(shutil.rmtree, d, True)
        tree = os.path.join(d, "console")
        shutil.copytree(CONSOLE_DIR, tree, ignore=shutil.ignore_patterns("__pycache__"))
        with open(os.path.join(tree, rel), "w", encoding="utf-8") as f:
            f.write(text.replace(old, new))
        return os.path.join(tree, "tests")

    def test_the_unmutated_code_passes_the_same_tests(self):
        tests = sorted({t for _, _, _, ts in MUTATIONS.values() for t in ts})
        r = run_tests(tests, HERE)
        self.assertEqual(r.returncode, 0, r.stderr[-3000:])

    def test_every_planted_fault_is_caught(self):
        prepared = {name: self.prepare(name) for name in MUTATIONS}
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = {name: pool.submit(run_tests, MUTATIONS[name][3], prepared[name]) for name in MUTATIONS}
        survivors = [name for name, fut in futures.items() if fut.result().returncode == 0]
        self.assertEqual(survivors, [], "these planted faults were not caught by their tests")


if __name__ == "__main__":
    unittest.main()
