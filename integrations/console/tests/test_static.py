"""The shipped page and script must never turn board text into markup or code."""

import os
import re
import unittest

import helpers
from helpers import STATIC

FORBIDDEN_JS = [
    ("innerHTML", re.compile(r"innerHTML")),
    ("outerHTML", re.compile(r"outerHTML")),
    ("insertAdjacentHTML", re.compile(r"insertAdjacent(HTML|Element|Text)")),
    ("document.write", re.compile(r"document\s*\.\s*write(ln)?\s*\(")),
    ("eval", re.compile(r"(?<![A-Za-z0-9_$.])eval\s*\(|\.eval\s*\(")),
    ("Function constructor", re.compile(r"new\s+Function\b|(?<![A-Za-z0-9_$.])Function\s*\(")),
    ("string timer", re.compile(r"set(Timeout|Interval)\s*\(\s*[\"'`]")),
    ("inline handler attribute", re.compile(r"setAttribute\s*\(\s*[\"']on", re.I)),
    ("style attribute", re.compile(r"setAttribute\s*\(\s*[\"']style", re.I)),
    ("srcdoc", re.compile(r"srcdoc", re.I)),
    ("createContextualFragment", re.compile(r"createContextualFragment")),
    ("DOMParser", re.compile(r"DOMParser")),
    ("javascript: URL", re.compile(r"javascript\s*:", re.I)),
    ("import()", re.compile(r"(?<![A-Za-z0-9_$.])import\s*\(")),
    ("importScripts", re.compile(r"importScripts")),
    ("WebSocket / EventSource", re.compile(r"new\s+(WebSocket|EventSource)\b")),
    ("sendBeacon", re.compile(r"sendBeacon")),
    ("XMLHttpRequest", re.compile(r"XMLHttpRequest")),
    ("absolute URL", re.compile(r"https?://", re.I)),
    ("localStorage", re.compile(r"localStorage|sessionStorage|document\.cookie")),
    ("location assignment", re.compile(r"(?<![\w$.])location\s*=[^=]|\blocation\s*\.\s*(href|assign|replace|hash|search|pathname)\s*=[^=]|\blocation\s*\.\s*(assign|replace)\s*\(")),
    ("window.open", re.compile(r"\bopen\s*\(\s*[\"'`]|\b(window|self|top|parent)\s*\.\s*open\s*\(|\bdocument\s*\.\s*open\s*\(")),
    ("dangerous createElement", re.compile(r"createElement(NS)?\s*\(\s*[\"'`]\s*(script|iframe|frame|object|embed|link|base|meta|form|style|svg|math|template|applet)", re.I)),
    ("src/href/action property assignment", re.compile(r"\.\s*(src|href|action|formAction|data|srcset|codebase|poster|ping)\s*=[^=]")),
    ("Worker", re.compile(r"\b(Shared)?Worker\s*\(|serviceWorker")),
    ("bracket access built from strings", re.compile(r"\[\s*[\"'`][^\]]*[\"'`]\s*\+|\[[^\]]*\+\s*[\"'`][^\]]*\]\s*(=[^=]|\()")),
    ("inner/outer HTML spelled in pieces", re.compile(r"[\"'`](inner|outer)[\"'`]|[\"'`]HTML[\"'`]|(inner|outer)[\"'`]\s*\+|\+\s*[\"'`]HTML", re.I)),
    ("element.setAttribute with a URL attribute", re.compile(r"setAttribute\s*\(\s*[\"'](href|src|action|formaction|data|srcset|xlink:href|poster|ping)[\"']", re.I)),
]

FORBIDDEN_HTML = [
    ("inline handler", re.compile(r"\son[a-z]+\s*=", re.I)),
    ("style attribute", re.compile(r"\sstyle\s*=", re.I)),
    ("inline script", re.compile(r"<script(?![^>]*\ssrc=)[^>]*>", re.I)),
    ("javascript: URL", re.compile(r"javascript\s*:", re.I)),
    ("absolute URL", re.compile(r"(src|href|action)\s*=\s*[\"']?(https?:)?//", re.I)),
    ("iframe/object/embed", re.compile(r"<(iframe|object|embed|form|base|meta[^>]+http-equiv)", re.I)),
    ("style element", re.compile(r"<style", re.I)),
]


def strip_comments(js: str) -> str:
    """Remove // and /* */ comments so a word in a comment is not a finding. Strings are kept:
    a forbidden token inside a string is still suspicious."""
    out, i, n = [], 0, len(js)
    quote = None
    while i < n:
        c = js[i]
        if quote:
            out.append(c)
            if c == "\\" and i + 1 < n:
                out.append(js[i + 1])
                i += 2
                continue
            if c == quote:
                quote = None
            i += 1
            continue
        if c in "\"'`":
            quote = c
            out.append(c)
            i += 1
        elif js.startswith("//", i):
            while i < n and js[i] != "\n":
                i += 1
        elif js.startswith("/*", i):
            j = js.find("*/", i + 2)
            i = n if j < 0 else j + 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def findings(text, rules):
    return [name for name, rx in rules if rx.search(text)]


def read(name):
    with open(os.path.join(STATIC, name), encoding="utf-8") as f:
        return f.read()


class ScannerSelfTests(unittest.TestCase):
    """The scanner has to catch the things it is meant to catch."""

    def test_flags_each_dangerous_construct(self):
        bad = {
            "innerHTML": "el.innerHTML = x;",
            "outerHTML": "el.outerHTML = x;",
            "insertAdjacentHTML": "el.insertAdjacentHTML('beforeend', x);",
            "document.write": "document.write(x)",
            "eval": "eval(code)",
            "Function constructor": "new Function('return 1')",
            "string timer": "setTimeout('alert(1)', 5)",
            "inline handler attribute": "el.setAttribute('onclick', x)",
            "style attribute": "el.setAttribute(\"style\", x)",
            "srcdoc": "f.srcdoc = x",
            "javascript: URL": "a.href = 'javascript:alert(1)'",
            "absolute URL": "fetch('https://evil.example/x')",
            "localStorage": "localStorage.setItem('a', b)",
            "DOMParser": "new DOMParser()",
            "location assignment": "location.href = x",
            "location assignment ": "location = x",
            "window.open": "window.open(u)",
            "dangerous createElement": "document.createElement('script')",
            "src/href/action property assignment": "img.src = x",
            "Worker": "new Worker(u)",
            "bracket access built from strings": "el['inner' + 'HTML'] = x",
            "inner/outer HTML spelled in pieces": "el[\"outer\" + \"HTML\"] = x",
            "element.setAttribute with a URL attribute": "a.setAttribute('href', x)",
        }
        for name, sample in bad.items():
            self.assertIn(name.strip(), findings(strip_comments(sample), FORBIDDEN_JS), sample)

    def test_does_not_flag_ordinary_code(self):
        ok = ("var x = items.map(function (i) { return i.id; }); el.textContent = x; setTimeout(tick, 5); "
              "var h = location.hash || ''; history.replaceState(null, '', '#/feed'); el.className = a + b; "
              "d.compare.warning ? 1 : 2; o[k] = v; var w = a[i + 1]; if (el.open) {} k === 'open'")
        self.assertEqual(findings(strip_comments(ok), FORBIDDEN_JS), [])

    def test_comments_are_not_findings_but_strings_are(self):
        self.assertEqual(findings(strip_comments("// innerHTML is banned\n/* eval( */ var a = 1;"), FORBIDDEN_JS), [])
        self.assertIn("innerHTML", findings(strip_comments("var s = 'x.innerHTML';"), FORBIDDEN_JS))

    def test_html_rules(self):
        bad = ['<a onclick="x()">', '<p style="color:red">', "<script>alert(1)</script>",
               '<script src="https://cdn.example/x.js"></script>', '<iframe src="/x">', "<style>a{}</style>"]
        for sample in bad:
            self.assertTrue(findings(sample, FORBIDDEN_HTML), sample)
        self.assertEqual(findings('<script src="/static/app.js" defer></script>', FORBIDDEN_HTML), [])


class ShippedFilesTests(unittest.TestCase):
    def test_app_js_has_no_dangerous_construct(self):
        # h() sets href itself, after its own check (see the test below); everything else is scanned
        # with the URL-attribute rule too, on the text outside h().
        found = findings(strip_comments(read("app.js")), FORBIDDEN_JS)
        self.assertEqual([f for f in found if f != "element.setAttribute with a URL attribute"], [])

    def test_index_html_has_no_inline_code_or_external_load(self):
        self.assertEqual(findings(read("index.html"), FORBIDDEN_HTML), [])

    def test_css_makes_no_external_request(self):
        css = read("style.css")
        self.assertIsNone(re.search(r"@import|url\s*\(\s*[\"']?(https?:)?//", css, re.I))

    def test_every_static_file_is_scanned(self):
        self.assertEqual(sorted(os.listdir(STATIC)), ["app.js", "index.html", "style.css"])

    def test_html_loads_only_our_own_files(self):
        html = read("index.html")
        refs = re.findall(r'(?:src|href)="([^"]+)"', html)
        for ref in refs:
            self.assertTrue(ref.startswith("/static/") or ref.startswith("#/"), ref)

    def test_script_is_strict_mode_and_uses_text_nodes(self):
        js = read("app.js")
        self.assertTrue(js.lstrip().startswith('"use strict";'))
        self.assertIn("createTextNode", js)
        self.assertIn("textContent", js)

    def test_hrefs_are_only_hash_routes(self):
        js = strip_comments(read("app.js"))
        # the helper drops any href that does not start with "#/"
        self.assertIn('v.indexOf("#/") === 0', js)

    def test_fetches_only_relative_paths(self):
        js = strip_comments(read("app.js"))
        for m in re.finditer(r'api\(\s*"([^"]+)"', js):
            self.assertTrue(m.group(1).startswith("/api/"), m.group(1))
        self.assertEqual(len(re.findall(r"\bfetch\s*\(", js)), 1)

    def test_h_is_the_only_place_that_sets_attributes_or_properties_by_name(self):
        js = strip_comments(read("app.js"))
        start = js.index("function h(tag, props)")
        end = js.index("function clear(el)")
        h_body = js[start:end]
        outside = js[:start] + js[end:]
        self.assertNotIn("setAttribute", outside)
        self.assertEqual(len(re.findall(r"setAttribute\(", h_body)), 2)  # href, and the allow-listed rest
        self.assertEqual(findings(outside, FORBIDDEN_JS), [])  # including setAttribute of URL attributes

    def test_h_only_sets_href_for_hash_routes_and_only_in_one_place(self):
        js = strip_comments(read("app.js"))
        hrefs = re.findall(r'setAttribute\(\s*"href"', js)
        self.assertEqual(len(hrefs), 1)
        m = re.search(r'k === "href"\)\s*\{\s*if \(typeof v === "string" && v\.indexOf\("#/"\) === 0\) el\.setAttribute\("href", v\);', js)
        self.assertIsNotNone(m)
        # every caller passes either a literal "#/..." or a value built from one
        for m in re.finditer(r"\bhref\s*:\s*([^,}]+)", js):
            expr = m.group(1).strip()
            self.assertTrue(expr.startswith('"#/') or expr.startswith("taskHref(") or expr.startswith("href"), expr)

    def test_createElement_is_called_with_a_variable_only_inside_h(self):
        js = strip_comments(read("app.js"))
        calls = re.findall(r"createElement\(([^)]*)\)", js)
        self.assertEqual(calls, ["tag"])
        start, end = js.index("function h(tag, props)"), js.index("function clear(el)")
        self.assertTrue(start < js.index("createElement(tag)") < end)

    def test_every_literal_tag_given_to_h_is_harmless(self):
        js = strip_comments(read("app.js"))
        tags = set(re.findall(r'\bh\(\s*"([a-z0-9]+)"', js))
        self.assertTrue(tags)
        self.assertLessEqual(tags, {"div", "span", "p", "a", "h2", "h3", "ul", "ol", "li", "table", "thead",
                                    "tbody", "tr", "th", "td", "b", "strong", "label", "select", "option",
                                    "input", "button", "section", "details", "summary"})
        self.assertIsNone(re.search(r"\bh\(\s*[^\"\s)]", js.replace("function h(tag", "")))  # no computed tag

    def test_polling_intervals_match_the_spec(self):
        js = read("app.js")
        self.assertIn("POLL_VISIBLE_MS = 2000", js)
        self.assertIn("POLL_HIDDEN_MS = 10000", js)
        self.assertIn("document.hidden", js)
        self.assertIn("visibilitychange", js)

    def test_feed_uses_a_cursor_not_a_full_reload(self):
        js = read("app.js")
        self.assertIn("after: st.cursor", js)
        self.assertIn("new event", js)


class QueueScreenTests(unittest.TestCase):
    def test_the_page_links_to_the_queue(self):
        self.assertIn('href="#/queue"', read("index.html"))

    def test_the_script_routes_and_fetches_the_queue(self):
        js = strip_comments(read("app.js"))
        self.assertIn('path === "#/queue"', js)
        self.assertIn('api("/api/queue")', js)
        self.assertIn("queueScreen", js)

    def test_the_queue_screen_names_every_section(self):
        js = read("app.js")
        for title in ("Runnable now", "Blocked", "Claimed", "Retrying", "Dead letter", "Awaiting approval"):
            self.assertIn('"%s"' % title, js)

    def test_the_new_attention_sections_are_rendered(self):
        js = read("app.js")
        for key in ("expired_leases", "dead_tasks", "blocked_forever"):
            self.assertIn(key, js)

    def test_the_queue_screen_polls_like_the_others(self):
        js = read("app.js")
        i = js.index("var queueScreen")
        self.assertIn("poll: async function", js[i:i + 200])

    def test_new_screens_add_no_dangerous_construct(self):
        js = strip_comments(read("app.js"))
        i = js.index("var queueScreen")
        self.assertEqual(findings(js[i:], FORBIDDEN_JS), [])

    def test_state_badges_have_styles(self):
        css = read("style.css")
        for cls in (".badge.dead", ".badge.awaiting_approval", ".badge.kind.fenced"):
            self.assertIn(cls, css)


if __name__ == "__main__":
    unittest.main()
