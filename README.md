# Tendr

**Agents remember what a tool call returned. Tendr keeps the process behind it alive.**

Coding agents have transcript memory: they can recall stdout, stderr, and exit codes from prior tool calls. But each shell tool call still runs as a fresh process. The transcript survives; the shell, REPL, database connection, dev server, or remote job usually does not.

Tendr supervises those processes as durable, named sessions. Your agent, or you, can reconnect later and continue from the same live runtime state.

## Why Tendr

- **The process, not just the transcript** — cwd, venvs, REPL variables, loaded tables, and open connections persist between every `exec`.
- **Walk away, come back** — sessions are durable and named; disconnect and resume hours or days later, state intact.
- **Runs where the work is** — drive a session on a remote box with `--host`; the same commands, over SSH.
- **Logs survive** — output lives on disk, not the agent's context. Crash- and compaction-proof.
- **Shells, REPLs, databases** — Bash, Python, IPython, DuckDB, and PowerShell stay warm, with structured output agents can parse.
- **Shareable** — no agent owns a session, so Claude, Codex, or you can drive the same one.

## Quickstart

```bash
tendr start --stdin dev -- bash      # a durable, supervised shell
tendr exec  dev -- cd repo
tendr exec  dev -- . .venv/bin/activate
tendr exec  dev -- pytest -x         # cwd + venv still active
tendr log   dev --tail 50            # on disk, survives crashes
printf 'y\n' | tendr push dev        # answer an interactive prompt
```

The *shell* lives inside Tendr and outlives every call. The same model works over SSH (`tendr --host box …`) and across REPL/DB lanes — `python3 -i`, `ipython`, `duckdb`, `pwsh` — where imported modules, loaded data, and open connections persist across every `exec`.

→ **[Full guide](docs/guide.md)** for the REPL lanes, remote sessions, and every command.

## Install

```bash
git clone https://github.com/grumpydevorg/tendr
cd tendr && cargo build --release   # → target/release/tendr
```

Prebuilt binaries and `cargo install tendr` arrive with the first release — see the [roadmap](docs/ROADMAP.md).

### Nix

```bash
nix build github:grumpydevorg/tendr#tendr   # or  nix build .#tendr  in a clone
nix run  github:grumpydevorg/tendr -- --version
```

The flake exposes `packages.<system>.tendr` for `aarch64-darwin`, `x86_64-linux` and
`aarch64-linux`, and `checks.<system>.tendr` is that same package — so building it runs the test
suite. It builds with nixpkgs' own rustc; `rust-toolchain.toml` remains the source of truth for
devenv and rustup, which are the paths that reach Windows.

The package also **ships the `using-tendr` agent skill**, installed alongside the binary at

```
$out/share/agent-skills/using-tendr/SKILL.md
```

from the same `src/embedded/SKILL.md` the binary embeds, so the packaged copy and the binary can
never describe different versions of tendr. A Nix-managed workstation links its agents' skill
directories straight at that path and never writes the file. Without Nix, `tendr skill install`
remains the way to put it in `~/.claude/skills/using-tendr/SKILL.md`.

## Docs

**[Documentation](docs/README.md)** · [Guide](docs/guide.md) · [Roadmap](docs/ROADMAP.md) · [Analytics recipes](docs/analytics-recipes.md) · [Architecture](docs/architecture/README.md)

## What Tendr is not

Not an AI agent, LLM framework, terminal UI, or security sandbox. It supervises sessions and owns process lifecycle — you bring the reasoning, the rendering, and the isolation.
