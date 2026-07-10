# Tender Guide

How to use Tender day to day — starting sessions, driving shells and REPLs,
observing long-running work, and reaching remote hosts. For the one-page pitch
see the [project README](../README.md); for how it works inside, see
[Architecture](architecture/README.md).

> Using Tender *from a coding agent?* The [`using-tender` skill](../.claude/skills/using-tender/SKILL.md)
> is a thin router that tells the agent the first rules to obey, then points it
> back to `tender guide` for version-matched detail.

## The model

A **session** is a long-lived child process — a shell, a REPL, a database
client, a script — that Tender supervises. You start it once; after that every
`tender exec` is a thin, one-shot client against that still-running process. The
process keeps its live state (cwd, env, activated venv, imported modules, loaded
tables, open connections) between calls; the transcript of what each call
returned is yours to keep, but the *process* is what Tender keeps alive.

Each session has a stable **name** and lives under a **namespace** (default:
`default`). Its durable truth is on disk — `meta.json` (state) and `output.log`
(append-only history) — so it survives a crash, a disconnect, or your agent's
context resetting. A per-session **sidecar** is the actual supervisor; the CLI
just talks to it.

## Install and build

The crate is `agenttender`; the installed binary is `tender`:

```bash
cargo install agenttender
tender --help
```

For unpublished local builds, build the `tender` binary from the repository:

```bash
cargo build --release --bin tender
install -m 0755 target/release/tender ~/.local/bin/
```

For Linux hosts from macOS, `cargo-zigbuild` is the simplest static-musl path:

```bash
brew install zig
cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl --bin tender
```

Musl builds avoid libc-version drift across remote hosts. Windows builds should
prefer the MSVC target on Windows itself; cross-compiling GNU Windows targets from
macOS/Linux may need an external binutils toolchain. For long remote builds, run
the build under Tender too, then follow with `tender log -f` and `tender wait`.

## Start a session

```bash
tender start --stdin dev -- bash          # a durable, supervised shell
```

- `--stdin` opens the pipe lane so `exec` can frame commands into the session. You almost always want it for interactive shells and REPLs.
- `-- <cmd> …` is the process to supervise. Everything after `--` is the child's argv.
- `--namespace <ns>` groups related sessions so `watch` can follow them together.
- `--replace` kills and restarts an existing session of the same name.
- `--timeout <sec>` kills the child if it overruns.

The child's kind (the **exec target**) is auto-inferred from the command —
`bash`/`sh` → shell, `duckdb` → DuckDB, `python3 -i`/`ipython` → Python REPL,
`powershell`/`pwsh` → PowerShell. Force it when inference is unclear:

```bash
tender start --stdin py --exec-target python-repl -- ipython --no-banner --no-confirm-exit
```

## Drive it with `exec`

```bash
tender exec dev -- cd repo
tender exec dev -- . .venv/bin/activate
tender exec dev -- pytest -x              # cwd + venv still active
```

Two rules that matter:

**`exec` takes argv, not a shell snippet.** `tender exec sh -- "cd /tmp && pwd"`
sends one argv element, not two shell statements. For multi-step shell work, use
separate `exec` calls or wrap explicitly:

```bash
tender exec sh -- bash -c 'cd /tmp && pwd'
```

**Gate success on the exit code, not on grepping stdout.** `exec` returns a JSON
envelope and the inner exit code propagates to `$?`:

```json
{"session":"dev","stdout":"…","stderr":"…","exit_code":0,"cwd_after":"repo","timed_out":false,"truncated":false}
```

```bash
tender exec ddb -- "$SQL" | jq -e '.exit_code == 0' >/dev/null || { echo FAIL; exit 1; }
```

Only one `exec` can be in flight per session — a second concurrent `exec` against
the same session fails with *"another exec is already running."* Start a second
session (`ddb2`, `py2`) for parallel inspection.

Large exec results are quiet. The JSON result still contains the command's
stdout/stderr and exit code, but the annotation line in `output.log` may degrade
when it would be too large. In that case Tender writes a compact
`exec_truncated` breadcrumb with `stdout_len`, `stderr_len`, `stdout_sha256`, and
`stderr_sha256` instead of raw payload. Find them with:

```bash
rg '"event":"exec_truncated"' output.log
```

## The REPL and database lanes

The same start/exec model turns any REPL into a durable session. In-memory
state — imported modules, loaded DataFrames, DuckDB tables, open connections —
survives across every `exec`.

### DuckDB

Structured JSON rows, ready to parse:

```bash
tender start --stdin ddb -- duckdb :memory:
tender exec  ddb -- "CREATE TABLE t AS SELECT range AS id, range * 2 AS val FROM range(5);"
tender exec  ddb -- "SELECT count(*), sum(val) FROM t;"     # → [{"count_star()":5,"sum(val)":"20"}]
```

### Python / IPython

The namespace persists:

```bash
tender start --stdin py -- python3 -i                       # or: ipython --no-banner
tender exec  py -- 'import pandas as pd; df = pd.read_csv("data.csv")'
tender exec  py -- 'print(df.describe())'                   # df still loaded
```

### PowerShell

`powershell` or `pwsh` — same clean-capture envelope, with two quirks worth
knowing:

- Each `exec` runs inside a fresh scriptblock scope, so variables need `$global:`
  to persist across calls (`$global:x = 42`). `Set-Location`, modules, and
  dot-sourced functions persist automatically.
- `Format-*` cmdlets throw inside the frame (no interactive host). Use
  `ConvertTo-Json` / `ConvertTo-Csv` / `Out-String` and pretty-print on the
  calling side.

## Answer prompts and attach

- **`tender push <name>`** feeds stdin to a session waiting on an interactive
  prompt: `printf 'y\n' | tender push dev`.
- **`tender attach <name>`** connects your terminal to the live session for
  hands-on interaction; detach and the session keeps running.

## Observe long-running work

No `ssh` + `tail` + `sleep` loops — Tender owns the read side:

```bash
tender status dev                 # current state
tender log    dev --tail 50       # last N lines (on disk, survives crashes)
tender log    dev -f              # follow
tender log    dev -s 5m           # since a time window
tender wait   dev --timeout 600   # block until it exits (propagates its code)
tender watch --namespace nightly --events --logs   # follow a whole namespace
```

`watch` takes a namespace, not a session name — it follows every visible session
(optionally filtered by `--namespace`).

## Lifecycle, batches, and hooks

```bash
tender kill  dev                  # stop a session
tender prune                      # remove terminated sessions (local-only)
tender run --detach ./job.sh      # one-shot convenience over `start` for scripts
```

Useful `start` / `run` flags for batch work:

- `--detach` — return immediately, leave it running.
- `--after <session>` — wait for other sessions to exit first (dependencies).
- `--on-exit <command>` — fire a hook when the child exits.
- `--replace` — restart an existing session of the same name.
- `--timeout <sec>` — kill on overrun.

## Reach remote hosts with `--host`

Put `--host` on the Tender command itself and the *same* commands run over SSH:

```bash
tender --host data-box start --stdin ddb -- duckdb /data/warehouse.duckdb
tender --host data-box exec  ddb -- "SELECT count(*) FROM read_parquet('s3://bucket/*.parquet');"
tender --host data-box log   ddb -f
tender --host data-box wait  extract_all --timeout 3600
```

`--host` carries `start`, `status`, `list`, `log`, `push`, `kill`, `wait`,
`watch`, `attach`, and `exec`. Remote `exec` ships the payload as one JSON frame
over the ssh stdin channel — it never traverses a remote shell, so there is no
nested-quoting layer to escape.

> **Remote-shell scope (read before `--host` to Windows).** Only `exec` uses the
> constant-argv frame transport, so it is safe against any remote shell. The other
> `--host` commands still reconstruct argv for a **POSIX** remote login shell.
> So today: **local Windows and remote `exec` are supported; general `--host`
> command forwarding remains POSIX-shell-only** — do not point general `--host`
> commands at a Windows host (cmd.exe / PowerShell) until the
> [remote frame transport](plans/active/00_remote-frame-transport.md) lands.
>
> *(A 2026-07-10 ARM-Windows smoke ran `start`/`kill`/`exec` with simple
> arguments — happy-path evidence that the mechanism runs, **not** proof of
> general Windows `--host` safety. Hostile or space-containing arguments were not
> exercised and remain exposed on the reconstructed-argv commands.)*

**`run`, `wrap`, `prune`, `query`, `guide`, and `skill`** are local-only.
Naming `--host` on them exits `2` with a ready-to-paste fallback:

```text
$ tender --host data-box run deploy.sh
error: 'run' is local-only and does not support --host
try:  ssh data-box 'tender run deploy.sh'
```

This is the workflow behind "leave long-running work on a remote box and come
back to it": start it under `--host`, disconnect, and reconnect later to `log`,
`status`, `wait`, or `exec` against the same live session.

On Windows hosts, the default ssh shell only matters when you write your own
`ssh host 'tender ...'` wrapper. `tender --host ... exec ...` uses a constant
remote argv (`tender exec --frame-from-stdin`) and sends the payload over stdin,
so the payload does not care whether the remote default shell is `cmd.exe` or
PowerShell. If you manually ssh-wrap local-only commands, quote for that remote
shell explicitly; for PowerShell-default hosts, `powershell -NoProfile -Command`
is often clearer than relying on implicit quoting.

This path was validated end to end on 2026-07-10 against a Parallels ARM Windows
VM over SSH, using the released Tender 0.2.0 x64 binary under Windows emulation.
Remote `exec` preserved PowerShell session state across frames, returned clean
structured JSON, and composed correctly with remote `start` and `kill`.

### Scripting: `exec --frame-from-stdin`

The transport `--host` uses is independently useful locally — pass the whole exec
request as one JSON frame on stdin so multi-line SQL/Python never fights argv
quoting:

```bash
jq -cn --rawfile sql query.sql '{v:1, session:"ddb", cmd:[$sql], timeout:300}' \
  | tender exec --frame-from-stdin
```

## Record where a session runs — `--boundary`

Optionally tag a session with the environment it runs in (host, container, VM,
pod) so `status` and analytics can tell a local session from one inside a
container on a remote box. Tender *describes* boundaries; it never manages them.

```bash
tender start job --boundary host:data-box -- make test
tender start dev --boundary container:my-image:latest --boundary-parent host:data-box -- bash
```

The boundary is authoritative in `meta.json` and is stamped, immutably, into the
run's lifecycle events for historical analytics. See
[the boundary plan](plans/completed/2026-07-10-boundary-metadata.md).

## Query the event log

Every supervised run emits a structured JSONL event stream. Point DuckDB at it
with `tender query` to answer questions across sessions — failure rates, longest
blocks, causal chains. See the [analytics recipes](analytics-recipes.md).

## Tender and Boo

Tender owns the **process**; [Boo](https://github.com/coder/boo) owns the
**screen**. They compose as a stack — supervise a Boo session with Tender for a
durable, accountable process while Boo drives and reads the live TUI. Tender does
rendered-screen reads for nobody and deliberately never will.

## See also

- [Architecture](architecture/README.md) · [Design principles](design-principles.md) · [Roadmap](ROADMAP.md)
- [`using-tender` skill](../.claude/skills/using-tender/SKILL.md) — the thin agent-facing router to `tender guide`
