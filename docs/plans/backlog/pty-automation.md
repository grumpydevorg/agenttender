---
id: pty-automation
depends_on:
  - pty-session-mode
links: []
---

# PTY Automation — Exclusive Agent Control for PTY Sessions

Add lease-backed agent control for PTY sessions so Tender can honestly enforce exclusive ownership of terminal input while still allowing human takeover when needed.

> **Status: deferred — correctness hardening, not roadmap (decided 2026-07-09).**
> Keep the current PTY behavior as-is. This plan is a lease system that hardens
> exclusive control of Tender's PTY **input** path; it is not strategic roadmap
> and not terminal *screen* automation. Build it only if real contention actually
> shows up — two agents, or a human and an agent, both writing into the same PTY
> and causing bad behavior. Absent that, today's one-writer-at-a-time behavior is
> good enough, and the effort (a new state machine + lease IPC + framed-push
> authorization) is not justified.
>
> **Already works today (shipped, no change needed):** `start --pty` backs a child
> with a real terminal; `push` writes raw bytes into the PTY; `attach` gives a
> human live control; while a human is attached, `push` is rejected; PTY output
> merges into `output.log` as `O` lines; generic `exec` is rejected on PTY sessions
> except the Python-REPL side-channel. The gap this plan closes is only that there
> is no *agent identity* on the input path — exclusivity is a label, not an
> enforced property. That is a latent correctness gap, not a felt pain.
>
> **Boundary with Boo:** this is input-ownership correctness, **not** screen
> automation. Rendered-screen reads (`peek`, `wait --text`/`--idle`, a VT/grid
> model) belong to Boo, the screen authority — Tender supervises the process and
> owns the durable record; Boo makes the terminal legible and controllable (see
> [boo-integration](boo-integration.md)). Between the two, Boo integration is the
> more immediately useful, docs-and-composition work; this PTY lease system is
> correctness hardening that waits for demand. The design below is preserved as-is
> for whenever that demand appears.

## Why

PTY session mode already exists for terminal-sensitive programs: password prompts, REPLs, TUIs, `ssh`, `psql`, and similar tools that do not fit the pipe+exec lane.

What is still missing is **authoritative ownership**.

Today:

- `start --pty` launches a real terminal
- `push` sends raw bytes into the PTY
- `attach` gives a human full terminal control
- PTY control state is persisted as `AgentControl` or `HumanControl`

But there is no agent identity on the input path. Any caller that can open the stdin transport can still push while the session is notionally "agent-owned." That makes exclusivity a label, not a system property.

This plan fixes that by making:

- PTY control a typed state machine
- push authorization happen at the transport boundary
- sidecar-owned PTY state the sole authority for transitions
- lease IPC a thin client over that state machine

## Scope

This plan covers **exclusive agent ownership for PTY input**.

It includes:

- data-carrying PTY control state
- sidecar-minted lease tokens
- framed push transport with authorization
- observable push rejection
- per-request lease IPC
- human preemption and restore
- lease expiry

It does **not** include observe mode. `tender log --follow --raw` remains the fallback for read-only transcript consumption. Observe-only socket subscribers should be a separate follow-on plan.

## Current State

Shipped in PTY session mode:

- `start --pty` spawns via `openpty`; sidecar owns the PTY master
- `push` writes bytes through the existing stdin transport into the PTY
- `attach` gives a human full terminal control over a Unix socket
- PTY output is tee'd to the attached human
- Unix only; no Windows ConPTY

Important current behavior:

- generic shell `exec` is rejected on PTY sessions
- Python REPL is the one PTY exec exception because it returns structured results through a side channel rather than transcript scraping

## Core Design

### 1. PTY control is a typed state machine

The control state must carry the lease data. Invalid combinations should be unrepresentable.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PtyControl {
    /// No agent has claimed exclusivity. Any actor may push.
    Open,

    /// An agent holds an exclusive lease. Only `holder` may push.
    AgentLeased { holder: LeaseHolder, lease: Lease },

    /// Human is attached. No push allowed.
    /// If `suspended_lease` is Some, restore on detach if still valid.
    HumanAttached { suspended_lease: Option<SuspendedLease> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHolder {
    pub agent_id: String,
    pub run_id: RunId,
    pub token: LeaseToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub acquired_at: EpochTimestamp,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendedLease {
    pub holder: LeaseHolder,
    pub lease: Lease,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseToken(uuid::Uuid);

impl LeaseToken {
    pub(crate) fn mint() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl std::fmt::Display for LeaseToken { /* hyphenated UUID */ }
impl std::str::FromStr for LeaseToken { /* validated UUID parse */ }

#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LeaseError {
    #[error("session is not PTY-enabled")]
    NotPty,
    #[error("lease already held by {agent_id}")]
    AlreadyHeld { agent_id: String },
    #[error("token does not match current lease")]
    WrongToken,
    #[error("a lease token is required in this state")]
    TokenRequired,
    #[error("no active lease")]
    NoLease,
    #[error("run_id does not match current run")]
    StaleRunId,
    #[error("human is currently attached")]
    HumanAttached,
    #[error("header too large or malformed")]
    BadHeader,
}
```

`LeaseToken` is opaque. It is minted by the sidecar and must be presented on future push, refresh, and release operations.

`LeaseToken`'s inner field is private. Minting happens through `pub(crate) fn mint()`. Outside the crate path that defines it, tokens are opaque and can only be obtained via `acquire`.

Use UUID v7 for token minting via `Uuid::now_v7()`.

`LeaseToken` implements `Display` using the hyphenated UUID form and `FromStr` with validation. CLI parsing should go through `FromStr` so malformed tokens fail before the request is sent.

All public enums introduced by this plan that are expected to grow carry `#[non_exhaustive]`: `PtyControl`, `LeaseError`, `LeaseAction`, and `LeaseOutcome`.

Tokens are cooperative-authority credentials, not cryptographic secrets. Deriving `Debug` is intentional and may expose tokens in logs. If the threat model tightens later, replace that with a redacting `Debug` implementation.

### 2. The state machine is the spec

Transitions are methods on `PtyControl`, not CLI conventions.

Required operations:

- `#[must_use] acquire(agent_id, run_id, ttl_secs, now) -> (next_state, LeaseToken)`
- `#[must_use] authorize_push(header, current_run_id) -> Result<()>`
- `#[must_use] refresh(token, now) -> next_state`
- `#[must_use] release(token) -> next_state`
- `#[must_use] human_attach() -> next_state`
- `#[must_use] human_detach(now) -> next_state`
- `#[must_use] reap_if_expired(now) -> Option<next_state>`

Illegal transitions return typed errors, not silent no-ops.

### 3. Push authorization happens at the transport boundary

Keep the existing stdin FIFO transport. Add a framed header as the first line on every push connection, then continue streaming raw bytes until EOF.

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct PushHeader {
    pub request_id: Uuid,
    pub run_id: RunId,
    pub agent_id: String,
    pub lease_token: Option<LeaseToken>,
}
```

`agent_id` is advisory. It exists for logging, telemetry, and error messages. The authoritative credential is `lease_token` bound to `run_id`. Two clients presenting the same token are intentionally indistinguishable to the sidecar.

Protocol:

1. Client opens stdin transport.
2. Client writes newline-terminated JSON `PushHeader`.
3. Client writes raw stdin bytes.
4. Sidecar reads the first line, parses the header, authorizes it against the current PTY state, then either:
   - accepts and forwards raw bytes into the PTY
   - rejects, writes a rejection record, and closes the transport

Sidecar caps the pre-newline header read at 4 KiB. Exceeding the cap is treated as `BadHeader`, recorded in `push_rejects/`, and the transport is closed.

Authorization rule:

- `Open`: any valid `run_id` may push; token is optional and ignored
- `AgentLeased`: header must include the current holder token and matching `run_id`
- `HumanAttached`: reject all push attempts

This is the critical enforcement point. Without this header, exclusivity is unenforceable.

### 4. Push rejection must be observable

The stdin transport is unidirectional, so rejection needs an out-of-band record that the client can read after a broken pipe.

Rejected pushes write:

```text
{session_dir}/push_rejects/{request_id}.json
```

Suggested shape:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct PushReject {
    pub request_id: Uuid,
    pub run_id: RunId,
    pub agent_id: String,
    pub error: String,
    pub rejected_at: EpochTimestamp,
}
```

Client behavior:

- write header
- stream stdin
- on `BrokenPipe`, look for `{request_id}.json`
- if present, surface `push rejected by sidecar: {error}`
- otherwise report an unexpected transport close

This keeps the FIFO transport, keeps file-IPC style consistent with the rest of Tender, and makes lease failures debuggable.

Reject files are swept by the sidecar on startup and on the existing sidecar poll cadence. Use a conservative TTL such as 5 minutes so clients have time to read their own rejection records without racing cleanup.

### 5. One owner, one write path

Delete ad hoc PTY control updates. The sidecar owns PTY control state through a single store.

```rust
pub struct PtyStateStore {
    state: Mutex<PtyControl>,
    session_dir: SessionDir,
}
```

The store is responsible for:

- holding the current in-memory `PtyControl`
- applying transitions
- reading full typed `Meta`
- mutating only the PTY field
- writing full typed `Meta` atomically

Every PTY state mutation goes through the store:

- attach listener
- push authorizer
- lease IPC handler
- expiry ticker

The stringly `update_pty_control` helper should be deleted.

Use `std::sync::Mutex` here. Lock scope is bounded to in-memory mutation plus the synchronous atomic `Meta` write; no `.await` is held across the lock.

Invariants the type system cannot encode should be defended with `debug_assert!` in store transition paths, for example that a suspended lease still refers to the current run and that a successful release ends in `Open`.

`write_meta_atomic` should use the same tmp-file plus `rename(2)` pattern already used elsewhere for `meta.json`. A crash before rename leaves the previous durable `Meta`, which is recoverable.

### 6. Lease IPC uses correlated per-request files

Keep file IPC, but make it race-safe.

```text
{session_dir}/lease/requests/{request_id}.json
{session_dir}/lease/responses/{request_id}.json
```

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub request_id: Uuid,
    pub run_id: RunId,
    pub agent_id: String,
    pub action: LeaseAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LeaseAction {
    Acquire { ttl_secs: u64 },
    Refresh { token: LeaseToken, ttl_secs: u64 },
    Release { token: LeaseToken },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub request_id: Uuid,
    pub outcome: LeaseOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LeaseOutcome {
    Granted { state: PtyControl },
    Denied { error: LeaseError },
}
```

`agent_id` is advisory here as well. It is recorded for attribution and diagnostics, but authorization is driven by `run_id` plus `lease_token`.

Sidecar behavior:

- poll `lease/requests/`
- process each request exactly once through `PtyStateStore`
- write matching response file
- delete request file

Client behavior:

- write one request file
- poll for matching response file
- read only its own `request_id`

This avoids collisions and stale-response confusion without adding a new daemon or socket.

If an acquire response is lost after the sidecar has already granted the lease, the minted lease remains valid until TTL expiry. The client will time out polling for its response and may retry `acquire`, which should be denied as already held until the orphaned lease expires. This is intentional; TTL is the recovery mechanism.

Orphaned request and response files are swept by the sidecar on startup and on the existing sidecar poll cadence using a conservative TTL.

On lease response timeout, the client re-reads session meta. If the run is `sidecar_lost`, surface that explicitly rather than reporting an unknown timeout. This applies to acquire, refresh, and release.

### 7. Human attach always preempts

Human `attach` must remain authoritative.

Rules:

- `Open` + human attach -> `HumanAttached { suspended_lease: None }`
- `AgentLeased` + human attach -> `HumanAttached { suspended_lease: Some(SuspendedLease { holder, lease }) }`
- human detach -> restore suspended lease if still valid, else `Open`

While in `HumanAttached`, all push attempts are rejected regardless of token.

### 8. Lease expiry is sidecar-owned

The sidecar periodically evaluates:

- active lease expiry
- suspended lease expiry during human attach

Lease requests, reject-file sweeps, and expiry checks all run on the existing sidecar poll interval. Do not add a separate timer thread for this plan.

Expiry transitions:

- expired `AgentLeased` -> `Open`
- expired suspended lease during `HumanAttached` -> drop the suspended lease; detach later restores to `Open`

No lease survives sidecar loss. If the sidecar dies, the run becomes `sidecar_lost` under the existing lifecycle rules.

## CLI Surface

### `pty-control`

```bash
tender pty-control acquire <session> --agent-id <id> [--ttl <seconds>] [--namespace NS] --json
tender pty-control refresh <session> --agent-id <id> --lease-token <token> [--ttl <seconds>] [--namespace NS]
tender pty-control release <session> --agent-id <id> --lease-token <token> [--namespace NS]
tender pty-control status  <session> [--namespace NS]
```

`acquire --json` should return the minted `lease_token` so callers can store it explicitly.

### `push`

`push` gains:

```bash
tender push <session> [--namespace NS] --agent-id <id> [--lease-token <token>]
```

Rules:

- on `Open`, `--lease-token` is optional
- on `AgentLeased`, `--lease-token` is required and must match
- on `HumanAttached`, push is rejected

Token caching is explicitly out of scope for v1. Callers hold the token and pass it back explicitly.

Exit codes should follow existing Tender CLI conventions. In addition, this plan assumes `2` for lease authorization failure (`WrongToken`, `TokenRequired`, `StaleRunId`, denied acquire/refresh/release) and `3` for state mismatch such as push during `HumanAttached`.

## State Diagram

```text
start --pty -> Open

Open
  -- acquire ----------------------> AgentLeased
  -- human attach -----------------> HumanAttached(suspended_lease=None)

AgentLeased
  -- refresh ----------------------> AgentLeased
  -- release ----------------------> Open
  -- expiry -----------------------> Open
  -- human attach -----------------> HumanAttached(suspended_lease=Some(...))

HumanAttached
  -- detach + suspended valid -----> AgentLeased
  -- detach + no suspended lease --> Open
  -- detach + suspended expired ---> Open
```

## What NOT to Build

- command boundary detection in PTY transcripts
- generic structured PTY exec for shell sessions
- multi-controller collaboration
- client-side token cache in v1
- observe mode in this plan
- browser terminal relay
- Windows ConPTY
- lease persistence across sidecar restart
- **rendered-screen automation of any kind** — `peek`, `wait --text`, `wait --idle`, or a VT/grid screen model. That is Boo's domain (screen authority); Tender owns process lifecycle + the durable record, not the rendered screen. See [boo-integration](boo-integration.md).
- **any terminal renderer in core** — no libghostty, no embedded VT engine. If a native screen layer is ever wanted it is a separate, deliberately-built satellite, not part of this lease work.

Clarification:

- Python REPL PTY exec already exists as a special side-channel protocol. This plan does not change that exception and does not generalize it to shell PTY sessions.
- This plan governs *who may write* to the PTY, never *what the screen shows*. The moment a requirement is about reading rendered output, it belongs to Boo, not here.

## Implementation Order

1. Extend `PtyControl` with data-carrying variants, plus `LeaseHolder`, `Lease`, and `LeaseToken`.
2. Add exhaustive pure state-machine tests for acquire, refresh, release, expire, attach, detach, and push authorization, including stale `run_id` rejection after `--replace`.
3. Add `PushHeader` framing and observable push rejection on the existing stdin transport.
4. Introduce `PtyStateStore` and migrate PTY state persistence to one typed write path.
5. Add per-request lease IPC and `tender pty-control`.
6. Add sidecar expiry ticker using `reap_if_expired`.
7. Rewrite attach listener transitions through `human_attach` / `human_detach` on the store.

Headline state-machine tests that must exist:

- `authorize_push_rejects_stale_run_id(valid_token, old_run_id) -> Err(StaleRunId)`
- `refresh_rejects_stale_run_id(valid_token, old_run_id) -> Err(StaleRunId)`

Supplement the enumerated matrix with `proptest` properties for `release -> Open`, refresh idempotency, acquire-after-expire, and `run_id` mismatch rejection regardless of token validity.

Observe mode, if still wanted after this lands, should be a separate plan.

## Acceptance Criteria

- exclusive PTY agent ownership is enforced at the push transport boundary
- stale agents from an old `run_id` cannot push, refresh, or release
- human attach always preempts agent control
- detach restores a suspended lease only if it is still valid
- push rejection is visible to the client, not just sidecar logs
- PTY control persistence uses one typed write path
- lease IPC is safe under concurrent callers because requests and responses are correlated by `request_id`
