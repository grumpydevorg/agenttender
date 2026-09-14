# PTY Lane

Tender has two execution lanes:

- pipe sessions: machine-friendly, `push` + `exec`, separate stdout/stderr
- PTY sessions: terminal-friendly, merged transcript, `push` + `attach`, no generic shell `exec`

This file describes the current PTY implementation, including the parts of
[cloud PTY control and replay](../plans/active/00_cloud-pty-control.md) that
have shipped: sidecar-enforced input ownership with controller epochs, the
attach handshake, takeover, and bounded viewer delivery. Recording, private
sockets, and the screen extension are still planned there.

```mermaid
stateDiagram-v2
    [*] --> AgentControl: tender start --pty

    AgentControl --> HumanControl: attach (hello, mode attach)\nwhile no human holds control
    HumanControl --> HumanControl: attach --takeover\nretires the previous client
    AgentControl --> HumanControl: attach --takeover\nrevokes an in-flight push
    HumanControl --> AgentControl: detach message\nor socket close

    AgentControl --> AgentControl: push accepted
    HumanControl --> HumanControl: push rejected
```

Current PTY rules:

- `start --pty` spawns the child under a PTY; the sidecar owns the PTY master,
  whose descriptions are nonblocking with poll-aware read and write halves
- one sidecar input writer owns the run's `InputArbiter` and the PTY input; every
  write is authorized against the current controller epoch on that thread, and
  control requests are served before queued input
- `push` claims control as an agent for the duration of one push connection
- `attach` must open with a v1 hello (`MSG_HELLO`); a plain attach is refused
  while a human holds control, and `--takeover` supersedes the holder, which
  receives `MSG_RETIRED` and is disconnected
- input queued by a superseded controller is never written; a revoked push is
  drained and recorded as `pty.input_revoked`
- while a human is attached, `push` is rejected
- PTY output is merged and recorded as `O` lines in `output.log`; capture only
  offers output to the attached viewer's bounded queue (8 MiB), drained by that
  viewer's own sender thread, so a viewer that stops reading is disconnected
  (releasing its control) instead of stalling capture and the child

PTY-specific I/O shape:

```mermaid
flowchart LR
    Push["tender push"] --> FIFO["stdin.pipe"]
    FIFO --> Forwarder["push forwarder (agent claim)"]
    Human["human terminal"] --> Attach["attach socket connections (hello, claim/takeover)"]
    Forwarder --> Writer["input writer (arbiter, epochs)"]
    Attach --> Writer
    Writer --> PTY["PTY master"]
    PTY --> Child["TTY-sensitive child"]
    Child --> PTY
    PTY --> Capture["capture thread"]
    Capture --> Log["output.log (tag O)"]
    Capture --> Attach
```

Important exception:

- generic shell `exec` is rejected on PTY sessions
- `ExecTarget::PythonRepl` is the implemented exception: it uses a side-channel result file instead of transcript scraping, so PTY Python REPL sessions can still support `exec`

Planned but not yet implemented (see the cloud PTY plan):

- exact output recording and replay, including catch-up for a disconnected
  viewer
- private, peer-verified attach sockets (the socket still lives in the system
  temporary directory)
- push over the attach socket with acknowledged outcomes, so `push` can report a
  revocation
- continuous resize forwarding and a detach escape in the `attach` CLI
