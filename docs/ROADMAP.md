# Roadmap

A short, public view of where Tender is going — directional, not a commitment.
Detail and history live in the [planning archive](plans/README.md); shipped work
is under [plans/completed/](plans/completed/).

## Now

- Remote frame transport — make `--host` genuinely cross-platform and close the Windows `--host` command-injection gap by sending every remote op as a typed frame ([plan](plans/active/00_remote-frame-transport.md))
- Shipped: native Windows CI (x64 + ARM64) gates Windows regressions
- Shipped: crate `agenttender` + binary `tender` on crates.io, attested multi-platform releases

## Next

- Boo integration: documented composition pattern, live validation still open
- Agent hook routing: small docs/glue around `tender emit`
- Query niceties: boundary helper columns if the SQL pattern proves common

## Later

- Content-addressable bundle / provenance work
- PTY input-lease hardening, if real contention appears

## Not In Core

- Terminal renderer / screen scraping — Boo territory
- Block-terminal UI, completion, and shell-vs-AI routing — downstream consumer policy
- Workflow scheduler / agent brain
- Container / Kubernetes lifecycle management
