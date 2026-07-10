---
name: using-tender
description: Use when an agent needs durable shells, REPLs, Python/IPython/DuckDB/PowerShell sessions, long-running commands, remote --host work, logs, watch/wait, or process state across tool calls. Not for ordinary one-shot file edits/searches.
---

# Using Tender

Tender keeps the process behind a tool call alive.

Before first use:
- `tender exec` takes argv, not a shell string. Use `-- bash -c '...'` for multi-step shell logic.
- Check `exit_code` / process status, not just stdout text.
- Use one in-flight `exec` per session.

For current, version-matched usage, run:

```bash
tender guide
tender guide install
tender guide exec
tender guide remote
tender guide python
tender guide duckdb
tender guide powershell
tender guide boundary
```
