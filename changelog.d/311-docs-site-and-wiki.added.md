Documentation site and wiki mirror (#311): `tools/docs_summary.py` stages `docs/` (plus the README,
CONTRIBUTING and `formats/`) with a generated `SUMMARY.md` and fails on a broken relative link; the
`docs-site` workflow builds it with a pinned mdBook on every pull request that touches the docs and deploys
to GitHub Pages from `main`; the `wiki-sync` workflow regenerates a small read-only wiki from the same
sources. New `docs/glossary.md`. The site and the wiki stay dormant until the owner enables Pages and
creates the first wiki page; see `docs/site-setup.md`.
