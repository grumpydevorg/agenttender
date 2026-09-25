# Plan Conventions

## Directory layout

```
docs/plans/
├── README.md
├── CONVENTIONS.md
├── active/          # ordered current work — numbered prefix filenames
├── backlog/         # unordered future work — slug filenames
├── completed/       # archived done work — date-prefixed filenames
└── specs/           # long-lived design docs (not queue items)
```

## Naming

| Location | Naming | Example |
|----------|--------|---------|
| `active/` | `NN_slug.md` — number sets priority | `00_windows-full-backend.md` |
| `backlog/` | `slug.md` | `pty-attach.md` |
| `completed/` | `YYYY-MM-DD-slug.md` | `2026-03-28-wrap.md` |
| `specs/` | `slug.md` | `tender-agent-process-sitter.md` |

Priority lives in filenames. `ls active/` is the ordered queue. README.md mirrors the active and backlog tables for discoverability (IDs, summaries, dependencies) but filenames are the source of truth for ordering.

## Frontmatter

Minimal. Only machine-useful fields:

```yaml
---
id: windows-full-backend       # semantic, never changes
depends_on: []                  # stable ids
links:                          # relative paths to related docs
  - ../completed/windows-full-backend.md
---
```

No `title` (heading is the title), no `created` (git history), no `closed` (archive location).

## Lifecycle

1. New work → create in `active/` with numbered prefix
2. Deferred work → create in `backlog/` with slug filename
3. Completed → `git mv` to `completed/`, add date prefix
4. Reorder → rename number prefix (e.g. `10_` → `05_`)

## Dependency references

`depends_on` uses stable `id` values only — not prose, not phase labels.

## This pattern is shared

Same frontmatter schema across: tendr, edge-platform, machine-learning, starling-edr. File layout varies per repo.
