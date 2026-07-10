---
id: remote-frame-transport
depends_on: []
links:
  - ../specs/event-protocol.md
  - ../specs/sidecar-control-protocol.md
  - ../../architecture/06-transport-boundaries.md
  - ../completed/2026-07-08-remote-exec-host-parity.md
---

# Remote Frame Transport — Make `--host` Genuinely Cross-Platform

Promote the `exec` frame transport's principle from one operation to the whole
remote surface: every `--host` command travels as a typed request over SSH
stdin, so **no user- or host-derived value is ever reconstructed into a remote
shell argv.** This closes a real command-injection vector on Windows and makes
the transport OS-neutral — without turning Tender into a daemon or RPC framework.

## Why — the security motivation

`src/ssh.rs:57` (`build_ssh_command`) POSIX-`shell_words::quote`s every arg of
the general remote commands (`start`, `status`, `list`, `log`, `push`, `kill`,
`wait`, `watch`, `attach`) and sends them to the remote **login shell**. The doc
comment (`src/ssh.rs:54`) already scopes this to POSIX shells and defers Windows.

The gap is exploitable, not cosmetic:

- Windows OpenSSH defaults to **`cmd.exe`**, which does not treat POSIX single
  quotes as quoting.
- `SessionName` / `Namespace` reject only slash, dot, whitespace, and a leading
  underscore — so `x&calc`, `x|whoami`, `x$(id)`, `x;ls` **all validate** (verified).
- Therefore `tender --host winbox status 'x&calc'` → `ssh -T winbox tender status
  'x&calc'` → cmd.exe splits on `&` → **`calc` executes**.

`exec` is immune because its remote argv is constant by construction
(`src/ssh.rs:96`). That is the proof the framed approach is correct; the older
reconstructed-argv transport is not.

## Design

### 1. A typed command IR (independent of Clap and SSH)

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "params", rename_all = "snake_case")]
pub enum RemoteOperation {
    Start(StartRequest), Status(StatusRequest), List(ListRequest),
    Log(LogRequest), Push(PushRequest), Kill(KillRequest),
    Wait(WaitRequest), Watch(WatchRequest), Exec(ExecRequest), Attach(AttachRequest),
}
```

Two constructors, one dispatcher:

```
Clap Commands ──TryFrom──▶ RemoteOperation ──┐
JSON frame ──deserialize─▶ RemoteOperation ──┴─▶ fn dispatch(RemoteOperation) -> Result<()>
```

This replaces the unsafe `Commands → Vec<String> → POSIX quote → remote shell →
Clap again` with `Commands → typed data → JSON → typed data → handler`.

**Do NOT serialize the Clap `Commands` enum.** It is a UI/parser type; wiring it
to the protocol would make every CLI refactor a wire change. The request structs
are stable protocol/domain DTOs.

### 2. Wire format — one hidden, constant entry point

```
ssh -T host tender _remote --frame-from-stdin
```

Nothing host- or user-derived appears after the SSH destination. The stream is:

```
4-byte big-endian header length
JSON header
optional raw body
```

Example header:

```json
{ "v": 1, "op": "start",
  "params": { "session": "build", "namespace": "default",
              "argv": ["powershell","-NoProfile"],
              "cwd": "C:\\Users\\rick\\project", "env": {"MODE":"release"},
              "stdin": true, "timeout": 300 } }
```

The length prefix exists because `push` needs raw bytes after the header. Cap
header size (~1 MiB) and **reject malformed / oversized / unsupported-version /
semantically-invalid requests before any side effect.** Unknown JSON *fields*
stay tolerated; unknown *versions* and *operations* are rejected.

### 3. Operation stream modes (fixed per op — no generic multiplexing)

| Mode | Operations | SSH stdin | SSH stdout/stderr |
|---|---|---|---|
| Request/response | start, status, list, kill, wait, exec | header, then EOF | existing output + exit code |
| Stream-out | log --follow, watch | header, then EOF | existing streaming output |
| Upload | push | header, then raw bytes | existing diagnostics |
| Duplex | attach | header, then attach frames | attach frames; errors on stderr |

`push` = length-prefixed header + raw body (sequential control/work framing, not
a multiplexed RPC). `attach` uses `ssh -T` (**not** `-t`): the local frontend
owns terminal raw-mode + resize; SSH carries bytes; the remote bridge connects
attach messages to the sidecar's Unix socket (or a future Windows named-pipe /
ConPTY channel). SSH transports bytes; it never becomes terminal authority.

### 4. What becomes cross-platform — and what does not

Cross-platform: the **transport** (macOS→Windows, Windows→Linux, Linux→Windows;
cmd.exe / PowerShell / bash / any configured OpenSSH shell — the remote shell
only ever sees the one constant safe command).

Still OS-specific (by design): **workload syntax** (a Windows target needs
Windows argv/paths; Linux needs Linux — Tender transports values exactly, it does
NOT translate bash↔PowerShell) and **process supervision** (Unix/Windows
backends). **`ExecTarget` stays session-local + authoritative in `meta.json`** —
the request says "run these fragments against this session"; the remote side
picks the existing PowerShell/POSIX/Python/DuckDB adapter from session metadata.

### 5. Not the sidecar control protocol

The frame terminates in the **remote Tender CLI**, which then uses today's local
files / named pipes / sockets / sidecar:

```
local tender → SSH → remote tender CLI → existing local IPC → sidecar
```

Preserves durable `meta.json` + logs, one lifecycle authority, **no listening
network daemon, no gRPC/Tokio/mTLS**, and the current output contracts. The full
[sidecar-control-protocol](../specs/sidecar-control-protocol.md) remains relevant
only if *local* correlated IPC genuinely needs it — this is not that.

## Implementation sequence (safe, incremental)

1. Add `RemoteOperation` request types + shared `dispatch`. Route local supported
   commands through the typed layer first. **No SSH behavior change yet.**
2. Add the framed codec + hidden `_remote` endpoint (partial-read handling, header
   limits, version check, semantic validation).
3. Move `start, status, list, log, kill, wait, watch, exec` to the frame.
   **`start` is the security priority** — its cwd, env, callbacks, and child argv
   are all currently shell-exposed.
4. Move `push` (header + raw body framing).
5. Build the `attach` bridge separately (Unix first; Windows needs ConPTY + a
   local named-pipe attach carrier).
6. Delete general POSIX remote-argv reconstruction. Keep shell quoting only for
   the human-facing copy/paste **fallback text**, never for execution.
7. Add native Windows x64 + ARM64 CI, including real cmd.exe and PowerShell
   OpenSSH tests.

## Required security tests

- Every remote op emits the identical constant SSH argv.
- Hostile values round-trip exactly: `` & | $ ; ( ) " ' ` ``, CR/LF, Unicode,
  Windows paths, spaces.
- `start` preserves arbitrary child argv, cwd, env, and callback strings.
- Malformed / oversized headers → no side effects.
- Unknown versions and operations fail clearly.
- `push` preserves arbitrary binary bytes without truncation.
- `log` / `watch` remain genuinely streaming.
- stdout / stderr / JSON / NDJSON / exit codes are byte-compatible with local.
- Old remote Tender → actionable "remote upgrade required" error.
- Native tests under Windows OpenSSH default cmd.exe **and** configured PowerShell.

## Name tightening (defense-in-depth, sequenced)

Target grammar: `[A-Za-z0-9][A-Za-z0-9_-]{0,254}`. But flipping
`SessionName::new()` immediately could orphan existing oddly-named sessions
(can't inspect/kill them). Safer:

1. **Immediately reject unsafe names on the legacy remote-argv path.**
2. Introduce the stricter grammar for **newly created** sessions.
3. Keep a narrowly-scoped legacy-name reader for local cleanup/migration.
4. Remove the remote restriction once framed transport lands.

Defense-in-depth only — `start` stays vulnerable via cwd/env/callback/child argv
until the frame replaces reconstruction. Not a substitute for the frame.

## Scope / non-goals

- Not a daemon, not a network listener, not gRPC/mTLS, not the sidecar RPC.
- No workload-syntax translation (bash↔PowerShell). Values transported verbatim.
- No new lifecycle authority — remote CLI reuses existing local IPC.

## Acceptance criteria

- All `REMOTE_COMMANDS` travel as typed frames; the remote SSH argv is constant
  and independent of any user/host value.
- The reconstructed-argv execution path is deleted (quoting survives only as
  copy/paste fallback text).
- Every "required security test" above passes.
- Windows CI (x64 + ARM64) runs real cmd.exe + PowerShell OpenSSH tests and gates
  regressions; a failed lane blocks the release.
- Until this lands, docs state the honest scope: local Windows + remote `exec`
  supported; general `--host` forwarding POSIX-shell-only.
