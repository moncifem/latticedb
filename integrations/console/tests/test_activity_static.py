"""The activity screens in app.js and style.css: same security rules as the rest (test_static.py scans the whole
file already), plus what is specific to the sparkline and to the wording."""

import re
import unittest

import helpers  # noqa: F401
from test_static import FORBIDDEN_JS, findings, read, strip_comments


class Sparkline(unittest.TestCase):
    def setUp(self):
        self.js = strip_comments(read("app.js"))

    def test_the_only_svg_is_made_inside_h_in_the_svg_namespace(self):
        self.assertEqual(len(re.findall(r"createElementNS\(", self.js)), 1)
        start, end = self.js.index("function h(tag, props)"), self.js.index("function clear(el)")
        self.assertTrue(start < self.js.index("createElementNS(SVG_NS, tag)") < end)
        self.assertEqual(re.findall(r'SVG_NS = "([^"]+)"', self.js), ["http://www.w3.org/2000/svg"])
        self.assertEqual(self.js.count("SVG_NS"), 2)  # the definition and its one use

    def test_only_three_element_names_are_svg(self):
        m = re.search(r"var SVG_TAGS = \{([^}]*)\}", self.js)
        names = sorted(re.findall(r"(\w+): 1", m.group(1)))
        self.assertEqual(names, ["rect", "svg", "title"])

    def test_svg_attributes_are_a_short_list_of_plain_ones(self):
        m = re.search(r"var PLAIN_ATTRS = \{(.*?)\};", self.js, re.S)
        keys = set(re.findall(r'"?(\w+)"?: 1', m.group(1)))
        self.assertEqual(keys, {"id", "title", "type", "name", "colspan", "scope", "role", "placeholder", "for", "class",
                                "x", "y", "width", "height", "viewBox", "preserveAspectRatio", "autocomplete", "maxlength"})

    def test_class_is_set_as_a_property_on_html_and_as_an_attribute_on_svg(self):
        self.assertIn('if (k === "class" && !isSvg) el.className = v;', self.js)

    def test_the_sparkline_has_a_text_alternative(self):
        i = self.js.index("function sparkline(")
        body = self.js[i:i + 900]
        self.assertIn('role: "img"', body)
        self.assertIn('"aria-label": label', body)
        self.assertIn('h("title", { text: label })', body)

    def test_the_scanner_exempts_only_the_exact_namespace_constant(self):
        rx = dict(FORBIDDEN_JS)["absolute URL"]
        self.assertFalse(rx.search('var SVG_NS = "http://www.w3.org/2000/svg";'))
        for foreign in ('var u = "http://evil.example/x";', '"https://www.w3.org/2000/svg.evil"',
                        '"http://www.w3.org/2000/svgx/evil"', '"http://www.w3.org/2000/evil"', '"http://www.w3.org/1999/xhtml"'):
            self.assertTrue(rx.search(foreign), foreign)

    def test_the_new_code_adds_no_dangerous_construct(self):
        start, end = self.js.index("function h(tag, props)"), self.js.index("function clear(el)")
        self.assertEqual(findings(self.js[:start] + self.js[end:], FORBIDDEN_JS), [])  # h() itself is checked by test_static


class Screens(unittest.TestCase):
    def setUp(self):
        self.js = read("app.js")
        self.code = strip_comments(self.js)

    def test_the_agent_screen_is_routed_and_fetched(self):
        self.assertIn('/^#\\/agent\\/(.+)$/', self.code)
        self.assertIn('api("/api/agent"', self.code)
        self.assertIn("makeAgentScreen", self.code)
        i = self.code.index("function makeAgentScreen")
        self.assertIn("poll: async function", self.code[i:i + 9000])

    def test_the_now_table_links_each_agent_to_its_screen_and_shows_the_activity_columns(self):
        self.assertIn('href: "#/agent/" + encodeURIComponent(name)', self.code)
        for title in ("Tool activity", "Doing now", "Last 60 min"):
            self.assertIn('"%s"' % title, self.js)

    def test_the_wording_says_what_stopped_not_that_the_agent_is_stuck(self):
        self.assertIn("no tool activity for ", self.js)
        self.assertIn("stuck?", self.js)
        for m in re.finditer(r"\bstuck\b(?!\?)", self.code):
            self.fail("the word stuck without a question mark: " + self.code[max(0, m.start() - 40):m.end() + 40])
        self.assertIn("it only says what stopped being reported", self.js)

    def test_the_needs_attention_strip_names_the_new_rule(self):
        self.assertIn("quiet_claims", self.js)
        self.assertIn("Claimed but no tool activity (heartbeat alive)", self.js)

    def test_the_agent_screen_states_what_is_and_is_not_stored(self):
        for text in ("reported by the agent's own hook", "never a command line, a file's content or a prompt",
                     "A compromised agent can report anything", "Active time (estimate)"):
            self.assertIn(text, self.js)

    def test_the_scorecard_shows_the_diagnostic_with_n(self):
        self.assertIn("Active time per done task (diagnostic)", self.js)
        i = self.js.index("Active time per done task (diagnostic)")
        near = self.js[i:i + 1500]
        self.assertIn("n = ", near)
        self.assertNotRegex(near, r"(?i)rank(?!ed)|best|worst|leaderboard")

    def test_filters_are_in_the_hash_and_validated_server_side(self):
        i = self.js.index("function makeAgentScreen")
        self.assertIn('history.replaceState(null, "", "#/agent/"', self.js[i:i + 1500])

    def test_styles_exist_for_the_new_badges_and_the_sparkline(self):
        css = read("style.css")
        for cls in (".badge.quiet", ".badge.stuckq", ".spark", ".spark .bar", ".spark .bar0"):
            self.assertIn(cls, css)
        self.assertNotRegex(css, r"url\(\s*[\"']?(https?:)?//")


if __name__ == "__main__":
    unittest.main()
