---
id: skill-claude-code
depends_on:
  - explicit-exec-targets
  - python-repl-exec
  - pty-session-mode
  - pty-automation
  - fleet-migration
links:
  - ../backlog/hermes-block-runtime-integration.md
---

# Agent Block-Runtime Skill for Tender

Write a skill that teaches hook-capable agents (Claude Code, Hermes, Codex, Cursor) how to use Tender as their supervised process execution substrate and event stream publisher.

## Goal

Ship a first-party skill that makes Tender the default runtime for any agent with lifecycle hooks:

- long-running commands
- background jobs
- log inspection
- stdin push and persistent shell workflows
- hook annotation with `wrap`
- supervised script execution with `run`
- structured event emission into Tender's block-runtime stream

**This is a documentation and workflow-packaging plan, not a product-code plan.**

**Canonical reference:** See `../backlog/hermes-block-runtime-integration.md` for the shell-bridge pattern that connects any agent's `hooks:` config to `tender event emit`. The skill below uses Claude Code as the primary example but the same bridge pattern works for Hermes (post_tool_call, pre_llm_call, on_session_start/on_session_end), Cursor (via its extension hooks), or any future agent that writes hook JSON to a subprocess.

## First Slice Goal

Land a usable skill that helps an agent choose the right Tender command without reading the repo docs.

First-slice output:

- skill file in the repo's expected skill layout
- trigger guidance for when to use Tender
- concise workflow examples
- recovery guidance for common failures

The first version should document the current product surface accurately:

- pipe `start --stdin` + `exec` is the default persistent-shell lane
- remote SSH command forwarding exists
- Unix PTY attach exists as a separate interactive lane
- PTY automation does not exist yet and must not be implied

## Required Content

The skill should cover:

- `tender start`
- `tender exec`
- `tender status` and `tender list`
- `tender log` and `tender watch`
- `tender push`
- `tender attach` for Unix PTY sessions
- `tender kill`
- `tender wrap`
- `tender run`
- session naming, namespaces, and idempotency semantics
- `--timeout`, `--on-exit`, and `--replace`
- Unix and Windows differences where they matter
- when not to use Tender for short one-shot commands

## Skill Structure

Recommended sections:

1. When to use Tender
2. Core commands
3. Preferred agent workflows
4. Failure recovery
5. Platform notes
6. Examples

## Example Workflows

The skill should include at least:

- start a long-running server, then watch logs
- run a script in the foreground and propagate its exit code
- start a persistent shell with `--stdin`, then use `exec` for structured commands
- start an interactive shell with `--stdin`, then `push` follow-up commands when `exec` is not the right fit
- use `wrap` from hooks and inspect annotations with `watch`
- replace an existing session safely
- kill a stuck run and verify terminal state

## Trigger Guidance

The trigger list should include phrases like:

- "run this in the background"
- "supervise this process"
- "keep this alive while I work"
- "check on that job"
- "stream the logs"
- "send input to the running shell"
- "run another command in that existing shell"
- "run this script but keep logs/state"

It should also say when not to trigger:

- a short foreground command is enough
- no session identity or later inspection is needed
- the user asked for plain shell execution rather than supervision

## Recovery Guidance

The skill should teach agents how to react to:

- `spawn_failed`
- `sidecar_lost`
- session conflict / idempotent start behavior
- namespace mistakes
- missing stdin transport for `push`
- kill vs force-kill behavior

## Deliverables

1. skill file under the repo's Claude/Codex skill layout
2. trigger phrase list
3. compact workflow guide
4. command examples using the current CLI
5. failure recovery guidance

## Implementation Tasks

1. Choose the skill file layout (`tender/SKILL.md` is likely cleaner than one flat file)
2. Draft trigger language and preferred workflows
3. Add concise command examples using the current CLI
4. Add recovery guidance for common failure modes
5. Review examples against the current tests and CLI behavior
6. Install the skill in the repo's expected skill location

## Acceptance Criteria

- a new agent can discover the right Tender command for common workflows without reading repo docs
- the skill reflects the current CLI, including `run`
- examples are valid on both Unix and Windows where possible
- the skill teaches idempotency and namespace usage correctly
- the skill teaches the lane split: pipe `exec` by default, PTY only for terminal-sensitive workflows
- the skill does not mention unimplemented features as if they already exist

## Depends On

All other backlog items. The skill documents the stable surface — write it last.

- `explicit-exec-targets` — persisted exec protocol selection
- `python-repl-exec` — side-channel exec for interpreter sessions
- `pty-session-mode` — persistent shell sessions
- `pty-automation` — agent-driven interactive programs
- `fleet-migration` — operational rollout (skill should reference fleet patterns)
