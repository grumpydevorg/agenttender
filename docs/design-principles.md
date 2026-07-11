# Tender Design Principles

This document is the design doctrine for Tender.

The architecture docs describe what the system is today. This file states the rules that should shape new features, reviews, and refactors.

Tender should stay a runtime substrate and session-control system. It should not drift into a workflow reasoning system or a domain-specific scheduler.

## The 5-Layer Stack

Think about Tender in five layers:

1. **Runtime substrate**
   Sidecar, child process, PTY/pipe transport, logs, process identity, kill/wait.
2. **Session control model**
   `start`, `exec`, `push`, `attach`, `status`, `wait`, `watch`, typed state machines.
3. **Composition primitives**
   `--after`, `--on-exit`, namespaces, and future pure projections such as `graph` or `check`.
4. **Workflow policy**
   Scheduling, retries, health rules, orchestration strategy, "what should be running."
5. **Domain tools**
   Compilers, extractors, CI coordinators, deployment tools, application-specific control loops.

Tender should own layers 1-3.

Be careful when adding layer-4 behavior.

Do not absorb layer 5 into Tender.

## The 7 Themes

### 1. Mechanism Over Policy

Tender should provide hard runtime guarantees and small composition primitives, not high-level workflow judgment.

Current example:

- `wait`, `watch`, `--after`, and `--on-exit` are runtime/control mechanisms.
- They do not decide what "healthy" means for a workspace, when a job should be retried, or what a deployment strategy should be.

Use in review:

- Ask: "Is this giving the caller a primitive, or is Tender deciding policy on the caller's behalf?"
- If the feature defines schedules, retry doctrine, or semantic health rules, it probably belongs above Tender.

### 2. One Authority Per Fact

Every durable fact should have one owner and one write path.

Current examples:

- The sidecar is the normal writer of lifecycle state in [`03-run-lifecycle.md`](architecture/03-run-lifecycle.md).
- CLI reconciliation is the sole lifecycle-writing exception: it either heals from the sidecar's terminal event or infers `SidecarLost`, recording provenance.
- PTY control writes go through a single typed path: the sidecar reads `Meta`, applies `set_pty_control`, then writes atomically — deleting ad-hoc `meta.json` mutation from multiple call sites.

Use in review:

- Ask: "Does this feature introduce a second writer for an existing fact?"
- If the answer is yes, stop and redesign until ownership is singular and explicit.

### 3. State Machine First, Protocol Second

For any control feature, define the typed states and legal transitions before defining CLI flags, files, sockets, or JSON payloads.

Current examples:

- The run lifecycle is a state machine first, documented in [`03-run-lifecycle.md`](architecture/03-run-lifecycle.md).
- The PTY lease review only became coherent after the design was rewritten around typed `PtyControl` transitions instead of starting from file IPC and CLI surface area.

Use in review:

- Ask: "If the transport disappeared, would the state model still be clear and testable?"
- If the answer is no, the protocol is pretending to be the spec.

### 4. Durable Truth, Derived Views

Tender should persist authoritative facts and derive views from them. It should not create new durable truth just to support a view.

Current examples:

- `meta.json` is the current run snapshot, `output.log` is the child-output
  record, and the segmented event log is lifecycle/provenance history.
- `status`, `list`, `log`, `watch`, `events`, and `query` are projections over
  those durable authorities.
- Any future `graph` or `check` command should remain a projection over session dirs, not add a second store.

Use in review:

- Ask: "Is this command a view over existing truth, or is it inventing a new shadow state?"
- If a projection needs its own on-disk authority, that is a sign it may not belong in Tender.

### 5. Separate Control Plane From Work Plane

Control operations and payload transport should stay separate.

Current examples:

- Control plane: `start`, `status`, `wait`, `watch`, `kill`, `attach`.
- Work plane: child stdin/stdout/stderr, PTY bytes, framed `exec` payloads.
- `watch` projects lifecycle and log events; it does not steer child stdin.

Use in review:

- Ask: "Is this feature crossing from control semantics into the child payload channel?"
- Do not smuggle workflow state through stdin, transcript parsing, or shell payload conventions when a control-plane field or state transition is the real model.

### 6. Inference Must Be Labeled

Tender sometimes knows facts directly and sometimes infers them. Those should not look identical.

Current examples:

- Direct: the sidecar writes a terminal state after observing child exit.
- Inferred: `SidecarLost` is concluded by reconciliation from released lock plus missing terminal state.

Shipped:

- `transition_provenance` distinguishes direct from inferred transitions and records the evidence for each; older metadata may omit it.

Use in review:

- Ask: "Is this an observation or an inference? Can downstream tooling tell which?"
- Prefer additive provenance fields over overloaded status names.

### 7. Composition Should Stay Shallow

Tender should support shallow composition of supervised runs, not become a full orchestration engine.

Current examples:

- `--after`, `--on-exit`, `wait`, `watch`, and namespaces are shallow and structural.
- They help compose workflows without defining a retry engine, scheduling system, or semantic planner.

Use in review:

- Ask: "Is this still structural composition, or are we building a scheduler?"
- If the feature wants DAG semantics, health rules, retries, or semantic planning, it likely belongs in a tool above Tender.

## Litmus Questions For New Features

Use this checklist during design and review:

- Does this introduce a new authority for an existing fact?
- Does the protocol come before the state machine?
- Are inferences distinguishable from observations?
- Is this a projection over durable truth, or is it creating a new truth?
- Does it cross from control plane into work plane?
- Is Tender providing a primitive, or deciding policy?
- Is the composition still shallow?

If a feature fails more than one of these checks, it likely belongs elsewhere or needs to be simplified.

## Anti-Patterns We Have Already Rejected

These are not abstract warnings. They came from real review cycles.

### Advisory ownership without transport enforcement

Rejected shape:

- PTY lease state that says an agent is exclusive, while `push` still accepts anonymous writes.

Why rejected:

- The label and the system behavior disagree.
- Authority must be enforced at the transport boundary, not merely recorded in metadata.

### Ad-hoc JSON surgery from multiple call sites

Rejected shape:

- Stringly `meta.json` patching from unrelated code paths.

Why rejected:

- It creates multiple writers for the same fact.
- It bypasses typed invariants and grows race surfaces silently.

### Protocol-first control features

Rejected shape:

- Starting a lease or control design with request files and CLI flags before defining the state machine.

Why rejected:

- It hides invalid states until review.
- It makes the transport look like the spec.

### Adding policy flags before verifying current mechanism

Rejected shape:

- Proposing a new `exec` strictness flag before checking whether `tender exec` already propagates inner exit codes.

Why rejected:

- It adds user-facing policy surface to solve a problem the current mechanism may already solve.
- Verification comes before feature growth.

## Applying This Doctrine

The goal is not to freeze Tender. The goal is to keep the system crisp as it grows.

Good new features usually look like this:

- they preserve one authority per fact
- they start from a typed state model
- they write durable truth once
- they expose projections without inventing shadow state
- they keep control semantics out of the work channel
- they give callers primitives rather than policy

When a proposal wants more than that, it is usually a sign that the feature belongs above Tender rather than inside it.
