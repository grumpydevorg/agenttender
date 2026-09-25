# PTY Lane

Tender has two execution lanes:

- pipe sessions: machine-friendly, `push` + `exec`, separate stdout/stderr
- PTY sessions: terminal-friendly, merged transcript, `push` + `attach`, no generic shell `exec`

This file describes the current PTY implementation on `main`, not the planned lease extension.

The remote-first successor is [cloud PTY control and replay](../plans/active/00_cloud-pty-control.md).
It is a plan, not shipped behavior: single-writer controller enforcement,
exact recordings, reconnect, and an optional external screen extension.

```mermaid
stateDiagram-v2
    [*] --> AgentControl: tender start --pty

    AgentControl --> HumanControl: tender attach\nor direct attach socket client
    HumanControl --> AgentControl: detach message\nor socket close

    AgentControl --> AgentControl: push accepted
    HumanControl --> HumanControl: push rejected
```

Current PTY rules:

- `start --pty` spawns the child under a PTY; the sidecar owns the PTY master
- `push` writes raw bytes into the PTY input path
- `attach` connects a human terminal over a Unix socket and steals control
- while a human is attached, `push` is rejected
- PTY output is merged and recorded as `O` lines in `output.log`

PTY-specific I/O shape:

```mermaid
flowchart LR
    Push["tender push"] --> FIFO["stdin.pipe"]
    FIFO --> Sidecar["sidecar forwarding thread"]
    Sidecar --> PTY["PTY master"]
    PTY --> Child["TTY-sensitive child"]
    Child --> PTY
    PTY --> Sidecar
    Sidecar --> Log["output.log (tag O)"]
    Sidecar --> Attach["attach socket relay"]
    Attach --> Human["human terminal"]
```

Important exception:

- generic shell `exec` is rejected on PTY sessions
- `ExecTarget::PythonRepl` is the implemented exception: it uses a side-channel result file instead of transcript scraping, so PTY Python REPL sessions can still support `exec`

Planned but not yet implemented:

This planned extension should follow Theme 3: State Machine First, Protocol Second; see [../design-principles.md](../design-principles.md).

- lease-backed exclusive PTY ownership
- token-authorized push headers
- PTY observe mode

Those are described in [../plans/backlog/pty-automation.md](../plans/backlog/pty-automation.md).
