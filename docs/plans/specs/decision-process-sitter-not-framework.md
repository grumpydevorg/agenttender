# Decision: Tendr Stays a Process Sitter

**Date:** 2026-04-03  
**Status:** Accepted

## Context

While designing exec targets for shells, REPLs, and databases, the question arose: should tendr learn to speak LLM protocols (OpenAI-compatible APIs, structured prompt/response cycles) as a native exec target?

Two approaches were considered:

**A) Native LLM target** — tendr learns HTTP, speaks OpenAI-compatible JSON, becomes an LLM orchestrator:
```bash
tendr start task --exec-target openai-compatible -- https://api.openai.com/v1
tendr exec task -- "analyze this code"
```

**B) Process sitter** — tendr manages processes that happen to call LLMs, without knowing what's inside:
```bash
tendr start task -- claude -p "analyze this code"
tendr start task -- bash -c 'curl -s api.openai.com/v1/chat/completions -d @prompt.json > result.json'
tendr start next --after task -- codex exec "process the results"
```

## Decision

**Tendr stays a process sitter.** It does not learn LLM protocols.

## Rationale

Wrapping LLM calls in processes already gives you everything tendr provides — lifecycle, dependencies, timeouts, output logs, namespaces, kill, wait, watch — for free.

The only things a native LLM target would add are framework concerns:
- Token/cost tracking
- Rate limit retry
- Structured prompt/response logging
- Model routing

The moment tendr understands what an LLM *is*, it stops being a process sitter and becomes an AI framework. That's a different tool with a different scope, different maintenance burden, and different users.

## How LLM Orchestration Works Today

Agent CLIs (`claude -p`, `codex exec`) are one-shot processes. Tendr orchestrates them via `--after` dependency chains:

```bash
tendr start research -- claude -p "find auth usages, write findings.txt"
tendr start impl --after research -- codex exec --full-auto "read findings.txt, refactor"
tendr start review --after impl -- claude -p "review the git diff"
```

For direct API calls, wrap in bash:

```bash
tendr start call -- bash -c 'curl -s https://api.openai.com/... -d @prompt.json > result.json'
```

For structured discovery of LLM runs, use annotations:

```bash
tendr start task --annotation type=llm-call --annotation model=gpt-5.2 -- ...
```

## Boundary

Tendr's job is:

| Tendr does | Tendr does not |
|-------------|-----------------|
| Start, supervise, kill processes | Understand what's inside them |
| Chain dependencies | Route between models |
| Track lifecycle state | Track tokens or cost |
| Log stdout/stderr | Parse LLM response structure |
| Timeout and retry at process level | Retry at API/rate-limit level |

## Consequences

- No `ExecTarget::OpenAiCompatible` or similar
- LLM orchestration is `--after` chains of one-shot agent processes
- Agent-specific concerns (cost, tokens, model selection) stay in the agent CLIs
- Tendr remains small, focused, and protocol-agnostic
