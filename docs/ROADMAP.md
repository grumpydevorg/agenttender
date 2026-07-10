# Roadmap

A short, public view of where Tender is going — directional, not a commitment.
Detail and history live in the [planning archive](plans/README.md); shipped work
is under [plans/completed/](plans/completed/).

## Now

- Package / release identity: crate `agenttender`, binary `tender`
- First release docs and install path

## Next

- Boo integration: documented composition pattern, live validation still open
- Agent hook routing: small docs/glue around `tender emit`
- Query niceties: boundary helper columns if the SQL pattern proves common

## Later

- Egui / block terminal
- Content-addressable bundle / provenance work
- Tender completer
- PTY input-lease hardening, if real contention appears

## Not In Core

- Terminal renderer / screen scraping — Boo territory
- Workflow scheduler / agent brain
- Container / Kubernetes lifecycle management
