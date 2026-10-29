import tempfile
import unittest
from pathlib import Path

import docs_summary as ds
import docs_wiki as dw
from test_docs_summary import write


def fixture(root: Path) -> None:
    write(root, "README.md", "![b](docs/assets/b.png)\n\n# Lattice\n\n[road](docs/roadmap.md) [lic](LICENSE) [c](CONTRIBUTING.md#setup)\n")
    write(root, "LICENSE", "x")
    write(root, "CONTRIBUTING.md", "# Contributing\n\nSee `docs/x.md` and [gl](docs/glossary.md).\n")
    write(root, "docs/roadmap.md", "# Roadmap\n\n[gate](gates/phase-1.md)\n")
    write(root, "docs/glossary.md", "# Glossary\n")
    write(root, "docs/gates/phase-1.md", "# Phase 1 status\n")
    write(root, "docs/gates/phase-10.md", "# Phase 10 status\n")
    write(root, "docs/gates/phase-2.md", "# Phase 2 status\n")
    arch = "# Arch\n\nintro\n\n"
    for i in range(1, 7):
        arch += f"## {i}. Section\n\ntext {i}\n\n```\n## not a heading\n```\n\n"
    write(root, "docs/ARCHITECTURE.md", arch)


class DocsWikiTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name).resolve()
        fixture(self.root)

    def tearDown(self):
        self._tmp.cleanup()

    def test_pages_and_banner(self):
        files, errors = dw.generate(self.root)
        self.assertEqual(errors, [])
        self.assertEqual(
            sorted(files),
            ["Architecture-overview.md", "Gates-and-board.md", "Glossary.md", "Home.md",
             "How-to-contribute.md", "Roadmap.md", "_Sidebar.md"],
        )
        for name, content in files.items():
            if name != "_Sidebar.md":
                self.assertTrue(content.startswith("> **Generated from docs/ in the main repository, edit there.**"), name)

    def test_home_drops_banner_image_and_maps_links(self):
        files, _ = dw.generate(self.root)
        home = files["Home.md"]
        self.assertNotIn("b.png", home)
        self.assertIn("(Roadmap)", home)
        self.assertIn("(How-to-contribute#setup)", home)
        self.assertIn(f"({ds.REPO_URL}/blob/main/LICENSE)", home)

    def test_roadmap_links_to_gate_become_urls(self):
        files, _ = dw.generate(self.root)
        self.assertIn(f"({ds.REPO_URL}/blob/main/docs/gates/phase-1.md)", files["Roadmap.md"])

    def test_architecture_excerpt_counts_only_real_headings(self):
        files, _ = dw.generate(self.root)
        arch = files["Architecture-overview.md"]
        self.assertIn("## 4. Section", arch)
        self.assertNotIn("## 5. Section", arch)
        self.assertIn("text 4", arch)
        self.assertNotIn("text 5", arch)
        self.assertIn("## not a heading", arch)

    def test_gates_page_is_numerically_ordered(self):
        files, _ = dw.generate(self.root)
        page = files["Gates-and-board.md"]
        self.assertLess(page.index("phase-1.md"), page.index("phase-2.md"))
        self.assertLess(page.index("phase-2.md"), page.index("phase-10.md"))
        self.assertIn(dw.BOARD_URL, page)

    def test_broken_link_is_an_error(self):
        write(self.root, "docs/glossary.md", "# Glossary\n\n[x](nope.md)\n")
        _, errors = dw.generate(self.root)
        self.assertEqual(len(errors), 1)

    def test_main_writes_and_replaces(self):
        out = self.root / "out"
        out.mkdir()
        (out / "stale.md").write_text("old")
        self.assertEqual(dw.main(["--root", str(self.root), "--out", str(out)]), 0)
        self.assertFalse((out / "stale.md").exists())
        self.assertTrue((out / "Home.md").exists())

    def test_main_does_not_write_on_error(self):
        write(self.root, "docs/glossary.md", "# Glossary\n\n[x](nope.md)\n")
        out = self.root / "out"
        self.assertEqual(dw.main(["--root", str(self.root), "--out", str(out)]), 1)
        self.assertFalse(out.exists())

    def test_strip_leading_images_keeps_other_text(self):
        text = "![a](x.png)\nkeep me\n\n# T\n"
        self.assertEqual(dw.strip_leading_images(text), "keep me\n# T\n")


if __name__ == "__main__":
    unittest.main()
