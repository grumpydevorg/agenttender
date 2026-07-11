# Key Flows

These are the load-bearing sequences in the current system. They show where the CLI exits, where the sidecar takes over, and which boundaries are file-, pipe-, or socket-based.

## `start`

```mermaid
sequenceDiagram
    participant Caller
    participant CLI as tender start
    participant Session as session dir
    participant Sidecar as tender _sidecar
    participant Child

    Caller->>CLI: start <session> -- <cmd...>
    CLI->>Session: create dir
    CLI->>Session: write launch_spec.json
    CLI->>CLI: create ready pipe
    CLI->>Sidecar: spawn detached sidecar with ready fd
    Sidecar->>Session: acquire lock
    Sidecar->>Session: read + delete launch_spec.json
    Sidecar->>Child: spawn child or PTY child
    Sidecar->>Session: write meta.json
    Sidecar-->>CLI: OK:<meta snapshot> over ready pipe
    CLI-->>Caller: print meta and exit
    Sidecar->>Session: supervise child until terminal
```

Notes:

- if dependencies are present, the sidecar writes `Starting` and signals readiness before waiting on `--after`
- if spawn fails, the sidecar returns a `SpawnFailed` snapshot over the ready pipe and the CLI exits non-zero

## `exec`

```mermaid
sequenceDiagram
    participant Caller
    participant CLI as tender exec
    participant Session as session dir
    participant FIFO as stdin.pipe
    participant Shell as running shell / repl
    participant Log as output.log

    Caller->>CLI: exec <session> -- <cmd...>
    CLI->>Session: read meta.json
    CLI->>Session: acquire exec.lock
    CLI->>CLI: frame command for exec_target
    CLI->>FIFO: write framed command
    FIFO->>Shell: inject command via stdin transport
    Shell->>Log: stdout/stderr or side-channel result
    CLI->>Log: wait for sentinel or result file
    CLI->>Log: append exec annotation
    CLI-->>Caller: print structured ExecResult
```

Notes:

- POSIX shell and DuckDB sessions signal completion with sentinel lines in `output.log` (DuckDB additionally drains trailing stderr)
- PowerShell and Python REPL sessions write a side-channel result file, `exec-results/<token>.json`
- timed-out exec holds `exec.lock` until the shell/repl finishes its frame, so a second exec cannot interleave into a busy session

## `kill`

```mermaid
sequenceDiagram
    participant Caller
    participant CLI as tender kill
    participant Session as session dir
    participant Sidecar
    participant OS as platform kill path

    Caller->>CLI: kill <session> [--force]
    CLI->>Session: read meta.json
    CLI->>Session: check lock held?
    alt sidecar alive
        CLI->>Session: write kill_request with run_id
        Sidecar->>Session: consume kill_request
        Sidecar->>OS: kill child tree via live kill handle
        Sidecar->>Session: write terminal meta.json
        CLI->>Session: poll meta.json until terminal
    else sidecar gone or unresponsive
        CLI->>OS: kill orphan by persisted ProcessIdentity
        CLI->>Session: poll meta.json for terminal write
    end
    CLI-->>Caller: print terminal meta or kill_sent
```

Notes:

- kill requests are bound to `run_id`, so stale control files from a previous run are ignored
- graceful vs forced kill classification is finalized by the sidecar using `kill_acted` / `kill_forced` breadcrumbs plus timeout state

## `attach` (local and remote)

```mermaid
sequenceDiagram
    participant Human
    participant CLI as tender attach
    participant SSH as ssh -t
    participant Socket as attach socket
    participant Sidecar
    participant PTY as PTY master

    alt local
        Human->>CLI: attach <session>
    else remote
        Human->>SSH: tender --host box attach <session>
        SSH->>CLI: remote tender attach <session>
    end
    CLI->>Socket: connect using a.sock.path breadcrumb
    Sidecar->>Sidecar: switch PTY control to HumanControl
    CLI->>Socket: MSG_RESIZE / MSG_DATA / MSG_DETACH
    Sidecar->>PTY: relay input and resize
    PTY->>Sidecar: terminal output
    Sidecar->>Socket: MSG_DATA back to attached client
    CLI-->>Human: raw terminal session
```

Notes:

- remote attach is still sidecar-local on the far host; SSH only carries the terminal
- attach uses `ssh -t`, unlike the non-interactive remote commands that use `ssh -T`

## `run`

`run` is a wrapper around `start` for scripts:

- parse directives from the script
- resolve interpreter / shell
- call the same `launch_session` path as `start`
- in foreground mode, follow `output.log` until terminal and then propagate exit code from final `meta.json`

## `wrap`

`wrap` is also a wrapper flow:

- must run inside a Tender-supervised process (`TENDER_RUN_ID` required)
- spawns the wrapped command directly
- captures stdin/stdout/stderr
- appends one `A` line to `output.log`
- exits with the wrapped command's exit code
