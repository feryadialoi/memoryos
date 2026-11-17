# Editing knowledge: own editor, git folders

Status: approved 2026-11-16 (E1 to E3), built as K1 to K4. Builds on `docs/pages-design.md` (ADR 0007). Goal is a quick
iteration to see how it feels, so the first cut avoids schema changes.

## 1. Goal

People edit knowledge pages in the editor they already use (VS Code, IDEA, vim, Obsidian), and a
team can keep curated knowledge in git with history and PR review. MemoryOS stays the copy agents
search: every synced file is an ordinary page version, split into sections, embedded, FTS indexed
and reranked exactly as `page_write` does today.

Not in scope: a rich editor inside `memoryos ui`. It would need a bundled editor library in a UI
that has no build step, and people would still be editing in a browser tab.

## 2. `memoryos page edit`

```
memoryos page edit deploy-runbook --scope shared:team [--approve]
```

1. Reads the current version (or starts empty when the page does not exist).
2. Writes it to a temp file `<slug>.md` (mode 0600) with front matter:
   ```
   ---
   title: Deploy runbook
   summary:
   ---
   ## Before you deploy
   ...
   ```
3. Runs the editor: `MEMORYOS_EDITOR`, then `VISUAL`, then `EDITOR`. GUI editors need their wait
   flag, such as `code --wait` or `idea --wait`. No editor set is an error naming these variables.
4. When the editor exits: no change means no write. Otherwise it writes a new version with
   `expected_version` set to the version it opened (none for a new page). `summary` in the front
   matter becomes the version summary.
5. On `VersionConflict` or any error the temp file is kept and its path printed, so no edit is lost.

`--approve` writes and approves in one call with the admin key, as `page write --approve` does.
Without it an edit to `shared:` or `user:` with an agent key waits for review, as today.

## 3. "Open in editor" in `memoryos ui`

A button on each page in the Knowledge view. It calls a new local route on the UI server, not the
proxy: `POST /local/pages/{scope}/{slug}/edit`, behind the same page token and host check.

- The UI server runs the section 2 flow on the same machine, with the key it already holds (the
  stored admin key when there is one, so the edit is approved; otherwise the agent key, so it waits
  for review).
- The request names only scope and slug. The editor command comes from the environment of
  `memoryos ui`, never from the request.
- The route returns at once with `editing`. When the editor closes the version is written and the
  page shows it on its next refresh. One edit per page at a time; a second click is refused.
- The proxy allowlist does not change: the browser still cannot write pages or facts directly.

## 4. Git folders: `sync` and `pull`

A folder maps to one scope. Each top-level `*.md` file is one page: file name without `.md` is the
slug, front matter `title` (or else the first `# ` line, removed from the body) is the title.
Files in subfolders and names that are not valid slugs are skipped and listed.

```
docs/knowledge/
  .memoryos-sync.json     # committed: slug -> {version, sha256 of the file}
  deploy-runbook.md
  writing-style.md

memoryos page sync docs/knowledge --scope shared:team    # files -> MemoryOS
memoryos page pull docs/knowledge --scope shared:team    # MemoryOS -> files
```

`sync`, per file, compared with the state file and the page's current version:

| File vs state | MemoryOS current vs state | Action |
|---|---|---|
| same | same | nothing |
| changed or new | same, or page absent | write with `expected_version` = state version, approved (admin key) |
| any | moved on (edited in MemoryOS) | stop for this page: "edited in MemoryOS since last sync, run pull" |

After writing it updates the state file. A secret masked on write is reported with the file name;
the state stores the file's hash, so the masked copy does not cause a rewrite on every sync. Pages
in the scope with no file are listed, and deleted only with `--prune`.

`pull` writes each page whose current version moved past the state (agent edits that were approved,
or edits made with `page edit`) back to its file with front matter, and updates the state. You
review the diff and commit it.

Keeping the search copy fresh: a git `post-commit` and `post-merge` hook running `sync`, and for
shared knowledge a CI step on merge to `main`. Until a sync runs, recall sees the previous version.

## 5. Naming

The UI tab "Pages" becomes "Knowledge". The REST routes, MCP tools (`page_read`, `page_write`,
`page_list`) and the `memoryos page` CLI keep their names, so existing agents and scripts keep
working.

## 6. Plan

| Task | Content |
|---|---|
| K1 | `page edit` with editor resolution, front matter, conflict handling; unit tests with a fake editor script |
| K2 | `page sync` and `page pull` with the state file; tests on a temp folder against the SQLite test server |
| K3 | UI: tab rename, "Open in editor" button, local edit route; route tests |
| K4 | README and SPEC section 8; a manual run with VS Code against the dev server |

## 7. Decisions needing approval

- **E1.** Sync state lives in a committed `.memoryos-sync.json` in the folder, not in the database.
  No migration, so it is quick to try. Cost: two clones syncing the same folder can disagree until
  one pulls. A later `origin` column on page versions can replace it.
- **E2.** `sync` refuses to overwrite a page edited in MemoryOS since the last sync, instead of
  letting git win. You run `pull`, merge in git, then `sync`.
- **E3.** "Open in editor" uses a temp copy, even for pages synced from git. Editing a git-managed
  page this way makes the next `sync` stop for it until you `pull`.
