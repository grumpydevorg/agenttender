---
id: agent-hook-routing
depends_on: []
links:
  - ../specs/event-protocol.md
  - ../specs/ecosystem-landscape.md
  - boo-integration.md
---

# Agent Hook Routing — teach agents to publish into the event stream

A small, docs-and-glue item: extend the shipped `using-tender` skill with one
section showing hook-capable agents how to route their lifecycle hooks into
Tender's event stream, so the events become consumable by analytics, UIs, and
terminal adapters.

> Collapses the two stale cards this replaces — `skill-agent-block-runtime`
> (an umbrella whose "teach agents the Tender CLI" core already shipped as the
> maintained `using-tender` skill) and `hermes-block-runtime-integration` (a
> pre-`event-protocol.md`, zero-code, self-superseded integration doc). The one
> live idea survives: **agent hooks → `tender emit` / `tender wrap --event
> hook.*` → event stream → Boo/Ghostty/UI/analytics consumers.**

## Why

The event protocol shipped, and `hook.` is deliberately an **unreserved** kind
prefix (event-protocol.md §1) — the conventional namespace for external
lifecycle-hook events (Claude Code, CI, other agents). What's missing is the
short, correct recipe telling an agent author how to publish there. The
`using-tender` skill is the right home; it already exists and is actively
maintained.

## Scope

Add one section to `using-tender` (`.claude/skills/using-tender/SKILL.md`),
using the **shipped** protocol names/schema only:

1. **The pattern.** A hook fires → the agent runs `tender emit` (standalone
   event) or `tender wrap --event hook.<name>` (dual-writes the authoritative
   event + its `output.log` A-line, linked by `event_id`). Events land in
   `~/.tender/sessions/<ns>/<session>/events/*.jsonl` with the v1 envelope
   (`kind`, `source`, `block_id`, `parent_id`, `data`, UUIDv7 `id`, RFC-3339 `ts`).
   Causal chaining is ambient via `TENDER_BLOCK_ID` / `TENDER_PARENT_EVENT_ID`.

   ```bash
   # from a post-tool-use hook
   tender emit --kind hook.post_tool_use --data '{"tool":"Bash","exit_code":0}'
   # or, wrapping a supervised command so its output is captured + linked
   tender wrap --event hook.pre_tool_use -- <cmd>
   ```

2. **One Hermes recipe (example, not an integration plan).** Show mapping a
   Hermes lifecycle hook to `tender emit --kind hook.*`, as a copy-paste config
   snippet. Verify the current Hermes hook names against its docs before
   writing (the old plan's hook inventory was flagged unverified). No code in
   either codebase.

3. **Consumer note.** State plainly that Boo / Ghostty / a future egui view /
   `tender query` analytics are **consumers** of `hook.*` events, not core
   Tender — routing/rendering lives in those adapters (see
   [boo-integration](boo-integration.md), and the Lane B/D split in
   [ecosystem-landscape.md](../specs/ecosystem-landscape.md)). Core stays narrow.

## Non-goals

- No changes to tender product code — this is skill documentation plus one recipe.
- No first-class "agent runtime integration" per external tool; agents publish via the generic `emit`/`wrap` surface.
- No reserved `hook.*` schema — `hook.` stays open vocabulary (event-protocol.md §1).

## Acceptance criteria

- `using-tender` gains a concise "publishing agent hooks into the event stream" section using only shipped verbs/fields.
- Exactly one worked Hermes recipe, with hook names verified against current Hermes docs.
- The Boo/Ghostty/UI-as-consumer boundary is stated (not implied as core).
- No stale envelope language (`tender event emit`, `parent_block_id`, ULID, daemon, `schema_version`).
