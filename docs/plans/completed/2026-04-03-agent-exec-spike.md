---
id: agent-exec-spike
depends_on: []
links: []
---

# Agent Orchestration Spike — Tender as Agent Pipeline

Spike to test whether tender's existing `--after` dependency chains work for orchestrating one-shot coding agents. No new exec target needed — both Claude Code and Codex are one-shot (run task, exit).

## Key Finding (Pre-Spike)

Both agents are one-shot:
- `claude -p "task"` → runs, exits
- `codex exec "task"` → runs, exits

Neither stays alive as a persistent session. The exec model doesn't apply. The orchestration model is dependency chaining — each agent is a separate tender session with `exec_target: None`.

## Hypothesis

Tender's existing primitives (sessions, `--after`, timeouts, namespaces, `wait`, `watch`) are sufficient to orchestrate multi-agent pipelines without any Rust changes.

## What to test

### 1. Basic `--after` chain with Claude Code

```bash
tender start research -- claude -p "find all auth usages in src/, write findings to findings.txt"
tender start impl --after research -- claude -p "read findings.txt and refactor the old auth calls"
tender start review --after impl -- claude -p "review the git diff for security issues"
```

Does the chain execute in order? Does each agent see the previous agent's file output?

### 2. Mixed-provider pipeline

```bash
tender start analyze -- claude -p "analyze the codebase, write analysis.md"
tender start implement --after analyze -- codex exec --full-auto "read analysis.md, implement the recommendations"
tender start verify --after implement -- claude -p "run tests and verify the changes"
```

Different agents, same working directory, chained via tender.

### 3. Parallel fan-out with fan-in review

```bash
tender start auth -- claude -p "refactor auth module"
tender start db -- codex exec --full-auto "refactor database layer"
tender start review --after auth --after db -- claude -p "review all changes for consistency"
```

Two agents work in parallel, a third waits for both.

### 4. Failure handling

What happens when a middle agent fails? Does `--after` propagate the failure? Does `--any-exit` allow the chain to continue?

### 5. Output sharing

Agents share a working directory, so file-based communication works. But can a downstream agent also read the upstream's tender output log?

```bash
tender log research  # Does this give useful context to pass to the next agent?
```

## Spike scope

- 2-4 hours, no Rust changes
- Shell scripts only
- Test with `claude -p` and `codex exec --full-auto`
- Document: what works, what breaks, what's missing

## Questions the spike should answer

1. Does `--after` chaining work reliably with one-shot agents?
2. Is file-based communication between agents sufficient, or do agents need access to upstream session logs?
3. What timeout values are practical for agent sessions?
4. Does parallel fan-out + fan-in review work?
5. What's missing from tender's current primitives for this use case?

## Not In Scope

- New exec targets or Rust changes
- Prompt engineering or agent behavior tuning
- Multi-model routing or cost tracking
- Building an AI framework

## Results (spike outcome)

Ported from `spike/agent-exec/README.md` when the spike scripts were retired
(2026-07-09) — this plan is now the single source of truth for the findings.

**Verdict: it works.** Tender's existing primitives (sessions, `--after`,
`--cwd`, timeouts, namespaces, `wait`, `watch`, `log -r`) are sufficient to
orchestrate multi-agent pipelines of one-shot coding agents (`claude -p`,
`codex exec`) with **no Rust changes needed**.

| Script (retired) | What it tested | Result |
|---|---|---|
| `01-basic-chain.sh` | linear 3-session chain: research → impl → review | PASS |
| `02-parallel-fan-out.sh` | two parallel sessions, one fan-in waiting for both | PASS |
| `03-failure-propagation.sh` | middle session fails, downstream behavior | PASS (documented) |
| `04-real-agent-chain.sh` | template for real claude/codex agents (echo stand-ins by default) | PASS |

Each script used a unique namespace (`spike-*-$$`) and cleaned up after itself;
`04` ran real agents under `USE_REAL_AGENTS=1`.

Key findings:

1. **`--after` chaining is reliable.** Linear A→B→C runs in strict order; each session waits for its dependency's terminal state before starting.
2. **Parallel fan-out + fan-in works.** Multiple `--after` flags AND-join (`--after auth --after db` waits for both); upstreams run in parallel.
3. **Failure propagation is well-designed.** A non-zero dependency puts downstream in `DependencyFailed` (`dep_reason: "Failed"`, warning, child never started) — the safe default; `--any-exit` opts into best-effort "run anyway".
4. **`--cwd` gives shared workdirs** — the natural file-based comms channel for coding agents, simpler than piping logs.
5. **`tender log -r`** strips the JSONL envelope → clean text to feed a downstream agent or human.
6. **Status reporting is clear** — `Exited`(`ExitedOk`/`ExitedError`+code), `DependencyFailed`(`dep_reason`+warnings), `Starting` while waiting, plus timestamps.

Answers to the spike questions:

1. `--after` chaining reliable with one-shot agents? **Yes.**
2. File-based comms sufficient? **Yes**, via `--cwd` shared workdirs.
3. Practical timeouts? ~**300 s** (5 min) per real-agent session via `--timeout`.
4. Parallel fan-out + fan-in? **Yes**, via multiple `--after`.
5. What's missing? Nothing blocking — see below.

Non-blocking improvement ideas (not built): a declarative `tender pipeline` DAG
spec; `--inject-dep-logs` to auto-prepend a dependency's output to a prompt;
`tender wait` forwarding the child's real exit code (currently 42 on any failed
dependency); a pipeline-level success/fail summary.
