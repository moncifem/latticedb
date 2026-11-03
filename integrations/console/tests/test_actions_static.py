"""The page and script of write mode: what they must do and what they must never do.

test_static.py already scans every line of app.js for markup and code injection; this file pins the rules that are
specific to decisions. They are checks on the text of the shipped files (no browser runs in CI): the behaviour of
the dialog is checked by hand in the demo (README, "Write mode") and the server side by test_actions_server.py.
"""

import re
import unittest

import helpers  # noqa: F401
from test_static import findings, read, strip_comments, FORBIDDEN_JS


def js():
    return strip_comments(read("app.js"))


class Token(unittest.TestCase):
    def test_the_token_lives_in_one_variable_and_one_header(self):
        text = js()
        self.assertEqual(len(re.findall(r'"X-Console-Write-Token"', text)), 1)
        self.assertEqual(len(re.findall(r'init\.headers\["X-Console-Write-Token"\] = writeToken;', text)), 1)
        self.assertEqual(len(re.findall(r'\bvar writeToken = "";', text)), 1)

    def test_the_token_is_never_stored_or_put_in_an_address(self):
        text = js()
        # no localStorage, sessionStorage, cookie or location write (h() sets href itself, after its own check)
        self.assertEqual([f for f in findings(text, FORBIDDEN_JS) if f != "element.setAttribute with a URL attribute"], [])
        for m in re.finditer(r"writeToken", text):
            line = text[text.rfind("\n", 0, m.start()) + 1: text.find("\n", m.end())]
            self.assertNotRegex(line, r"(fetch|history\.|\+ ?writeToken|writeToken ?\+|textContent ?= ?writeToken|console\.|body\.\w+ = writeToken)", line)

    def test_the_token_box_is_a_password_field_that_is_not_remembered(self):
        text = js()
        self.assertIn('type: "password"', text)
        self.assertIn('autocomplete: "off"', text)

    def test_a_refused_token_is_forgotten(self):
        text = js()
        i = text.index("if (e.status === 401)")
        self.assertIn('writeToken = "";', text[i:i + 120])

    def test_reload_forgets_it(self):
        # nothing reads it back from anywhere: the only assignments are the empty initial value, forgetting, and the box
        assigns = re.findall(r"writeToken = ([^;]+);", js())
        self.assertEqual(sorted(assigns), ['""', '""', '""', "v"])


class Requests(unittest.TestCase):
    def test_there_is_one_fetch_and_one_post(self):
        text = js()
        self.assertEqual(len(re.findall(r"\bfetch\s*\(", text)), 1)
        self.assertEqual(len(re.findall(r'init\.method = "POST"', text)), 1)

    def test_a_post_carries_json_and_only_goes_to_the_two_routes(self):
        text = js()
        self.assertIn('init.headers["Content-Type"] = "application/json";', text)
        posts = re.findall(r'postJson\(\s*"([^"]+)"', text)
        self.assertTrue(posts)
        self.assertLessEqual(set(posts), {"/api/act", "/api/act/bulk"})

    def test_every_decision_carries_a_request_id_made_by_the_browser_crypto(self):
        text = js()
        self.assertIn("crypto.getRandomValues", text)
        self.assertNotIn("Math.random", text)
        self.assertGreaterEqual(len(re.findall(r"request_id: ", text)), 2)

    def test_one_request_id_per_dialog_so_a_second_press_repeats_instead_of_doubling(self):
        text = js()
        i = text.index("function decideDialog(")
        j = text.index("openDialog(", i)
        self.assertIn("var rid = newRequestId();", text[i:j])
        k = text.index("function bulkDialog(")
        self.assertIn("var rid = newRequestId();", text[k:k + 300])
        self.assertIn("task: item.id, request_id: rid }", text[i:k])  # and the body really uses it
        self.assertIn("request_id: rid, confirm: dry.count", text[k:])
        self.assertEqual(text.count("newRequestId()"), 2 + 1)  # the two dialogs and the definition

    def test_the_actor_is_never_sent_by_the_page(self):
        text = js()
        i = text.index("function decideDialog(")
        sends = text[i:text.index("function card(")]
        self.assertNotRegex(sends, r"\b(actor|author|by)\b\s*[:=]")

    def test_there_is_no_bulk_approve(self):
        text = js()
        self.assertEqual(sorted(re.findall(r'bulkDialog\("([a-z]+)"\)', text)), ["cancel", "requeue"])
        self.assertNotRegex(text, r'bulkDialog\(\s*"(approve|reject)')

    def test_bulk_runs_a_dry_run_first_and_confirms_with_its_count(self):
        text = js()
        i = text.index("function bulkDialog(")
        body = text[i:text.index("function card(")]
        self.assertIn("dry_run: true", body)
        self.assertIn("dry_run: false", body)
        self.assertIn("confirm: dry.count", body)
        self.assertLess(body.index("dry_run: true"), body.index("dry_run: false"))


class Decisions(unittest.TestCase):
    def test_every_action_has_a_button_text_and_a_done_text(self):
        text = read("app.js")
        for a in ("approve", "reject", "cancel", "requeue", "priority", "note"):
            self.assertRegex(text, r"\b%s: \"" % a)

    def test_approve_sends_the_hash_the_card_showed(self):
        text = js()
        self.assertIn('if (action === "approve") body.sha = item.sha;', text)

    def test_reject_and_note_require_a_comment_in_the_dialog_too(self):
        text = js()
        i = text.index('} else if (action === "reject") {')
        self.assertIn("field.required = true;", text[i:i + 120])
        j = text.index('} else if (action === "note") {')
        self.assertIn("field.required = true;", text[j:j + 120])

    def test_approve_and_reject_are_disabled_for_the_task_you_produced(self):
        text = js()
        self.assertIn('var blocked = (a === "approve" || a === "reject") && item.you_produced;', text)
        self.assertIn("disabled: blocked", text)

    def test_cancel_and_requeue_ask_before_acting(self):
        text = js()
        for a in ("cancel", "requeue", "reject", "approve"):
            self.assertIn('action === "%s"' % a, text)
        self.assertIn("decideDialog(item, a)", text)
        self.assertNotRegex(text, r'postJson\("/api/act", \{ action: "(cancel|requeue)')  # only from the dialog's confirm

    def test_a_write_without_the_token_asks_for_it_instead_of_sending(self):
        text = js()
        self.assertEqual(len(re.findall(r"if \(needToken\(\)\) return;", text)), 2)
        self.assertIn('"Paste the write token first', text)

    def test_the_result_is_shown_with_the_event_id_and_the_list_refreshes(self):
        text = js()
        self.assertIn("afterDecision(a.message", text)  # the server's message names the event id
        i = text.index("function afterDecision(")
        self.assertIn('inboxScreen.sig = "";', text[i:i + 200])
        self.assertIn("screen.poll()", text[i:i + 300])

    def test_the_demo_is_labelled_on_the_toast(self):
        self.assertIn('writes.demo ? "DEMO, simulated: " : ""', js())


class Dialog(unittest.TestCase):
    def test_it_is_a_labelled_modal_that_closes_on_escape_and_keeps_focus_inside(self):
        text = js()
        for needle in ('role: "dialog"', '"aria-modal": "true"', '"aria-labelledby": "dlg-title"', 'e.key === "Escape"',
                       'e.key !== "Tab"', "opener.focus()", "(input || ok).focus()"):
            self.assertIn(needle, text)

    def test_it_is_made_with_dom_calls_only(self):
        text = js()
        i = text.index("function openDialog(")
        body = text[i:text.index("function postJson(")]
        self.assertNotRegex(body, r"innerHTML|insertAdjacent|document\.write|createElement")
        self.assertIn("document.body.appendChild(bg)", body)

    def test_the_text_of_the_task_goes_in_through_text_only(self):
        text = js()
        self.assertIn('h("div", { class: "panel text dlg-body", text: item.body', text)


class Page(unittest.TestCase):
    def test_the_inbox_is_linked_and_routed(self):
        self.assertIn('href="#/inbox"', read("index.html"))
        self.assertIn('path === "#/inbox"', js())
        self.assertIn("screen = inboxScreen;", js())

    def test_the_write_panel_and_toast_start_hidden(self):
        html = read("index.html")
        self.assertRegex(html, r'<section id="write-panel"[^>]* hidden>')
        self.assertRegex(html, r'<div id="toast"[^>]* hidden>')
        self.assertIn('aria-live="polite"', html)

    def test_the_header_says_read_only_until_the_server_says_otherwise(self):
        self.assertIn('id="mode-tag">alpha, read-only<', read("index.html"))
        self.assertIn('"alpha, writes on as " + writes.actor', js())

    def test_the_footer_no_longer_claims_the_console_only_reads(self):
        self.assertIn("unless it was started with --allow-writes", read("index.html"))

    def test_the_inbox_names_what_puts_a_task_there(self):
        self.assertIn("What puts a task here", js())

    def test_an_unchanged_list_is_not_redrawn_under_the_pointer(self):
        text = js()
        i = text.index("var inboxScreen")
        self.assertIn("if (sig === this.sig) return;", text[i:])

    def test_phone_width_and_keyboard_focus_are_styled(self):
        css = read("style.css")
        self.assertIn("@media (max-width: 600px)", css)
        self.assertIn(":focus-visible", css)
        for sel in (".modal-bg", ".modal", ".toast", ".card", ".btn", ".write-panel"):
            self.assertIn(sel, css)

    def test_the_content_security_policy_is_not_loosened(self):
        from console.server import CSP
        self.assertEqual(CSP, "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; "
                              "img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")


if __name__ == "__main__":
    unittest.main()
