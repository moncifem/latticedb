# Docs site and wiki: owner setup

The workflows `docs-site.yml` and `wiki-sync.yml` publish the documents in `docs/` as a website and as
a small read-only wiki. Neither works until the repository owner does the two one-time steps below.
Agents and contributors cannot do them: they are repository settings.

**Until step 1 is done the site is not live**, and the link
`https://moncifem.github.io/latticedb/` returns 404. Until step 2 is done the wiki mirror skips with a
notice and does nothing.

## 1. Enable GitHub Pages (site)

1. Open Settings, Pages, in the repository.
2. Under "Build and deployment", set Source to **GitHub Actions**. Do not pick a branch.
3. Run the workflow once: Actions, `docs-site`, Run workflow on `main` (or merge any change under `docs/`).
   The `deploy` job uses the `github-pages` environment, which GitHub creates when Pages is enabled.
   If you restrict that environment's deployment branches, allow `main`.
4. The first successful run prints the site URL in the job summary. The expected URL is
   `https://moncifem.github.io/latticedb/`.

Until Pages is enabled, the `deploy` job fails at `actions/configure-pages` with a "Pages not enabled"
error. The build job, which runs on every pull request, is unaffected and is the check that matters
for contributors.

## 2. Initialize the wiki

GitHub creates the wiki's git repository (`latticedb.wiki.git`) only when the first page is saved in the
web UI; a repository with the wiki enabled but no page has nothing to push to
([GitHub: adding or editing wiki pages](https://docs.github.com/en/communities/documenting-your-project-with-wikis/adding-or-editing-wiki-pages)).

1. Open Settings, General, Features, and confirm "Wikis" is ticked. (Optionally also tick "Restrict
   editing to collaborators only".)
2. Open the Wiki tab and choose "Create the first page". Save it with any content, for example the
   title `Home` and the text `placeholder`. The workflow overwrites it.
3. Run Actions, `wiki-sync`, Run workflow on `main`. From then on every push to `main` that changes
   `docs/**` regenerates the pages.

`wiki-sync` pushes with the workflow's own `GITHUB_TOKEN` (`contents: write` on that one job). No
personal access token or secret is needed. If the push is ever rejected with a 403, the log says so;
GitHub's documentation does not promise this behaviour for every account type, so that would be the
point to add a token secret instead. Until step 2 is done the workflow detects that the wiki repository
does not exist, prints a notice and ends successfully.

## What not to do

- Do not edit wiki pages by hand. The next sync overwrites them. Edit `docs/` and open a pull request.
- Do not enable deployments from a branch; the workflow is the only publisher.
