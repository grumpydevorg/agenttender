---
id: tender-completer
depends_on:
  - event-emit-primitive
links:
  - ../specs/tender-as-block-runtime.md
  - egui-block-terminal.md
---

# `tender-completer` — Agentic Input Classification + Command Completion

A Rust library that takes a string the user is typing and returns: (a) whether it's a shell command or natural language, and (b) ranked completion suggestions. Mirrors Warp's input-classifier + completer architecture as a four-sub-module crate, usable by `tender-blocks-egui` and any other Tender-aware UI.

Lives in a separate workspace member (`tender-completer/`) — does NOT pull ML, JS, or completion deps into core Tender.

## Why

The most useful day-to-day Warp feature is its inline completions:

- you type `git ch` → it suggests `checkout`, `cherry-pick`, with descriptions
- you type `do` → suggests `docker run`, `docker ps`, with flag awareness
- you type `kubectl get pods --namespace ` → suggests namespaces from your current context
- you type `how do I list all docker containers using > 1GB` → routes to AI (this is the "agentic" part)

None of this is magic; Warp does it with three boring, well-engineered pieces stacked. We can ship the same primitives as a Rust library that any Tender-aware UI consumes. The first consumer is `tender-blocks-egui`; second is likely a `tsh` CLI; third would be any IDE plugin or shell wrapper that wants Tender-grade completions.

The work is mostly **glue and curation**, not new research. Warp open-sourced their implementation in April 2026; we can study their architecture, vendor what's MIT-licensed, and avoid the AGPL pieces. The TypeScript-based command spec ecosystem (Fig autocomplete specs, ~5000 commands) is independently MIT-licensed and is the multiplier.

## The four sub-modules (mirroring Warp's separation)

### a) `tender-completer-classifier` — Shell vs AI router

A 2-state classifier that decides:

```rust
pub enum InputType {
    Shell,    // route to the shell / command completer
    AI,       // route to the AI assistant
}
```

Two cooperating classifiers:

1. **Heuristic** — cheap, instant, deterministic.
   - Stemmed-English-word ratio: if the input is mostly common English stems, lean AI.
   - Token shape: if tokens look like flags (`-x`), paths (`./foo`, `~/bar`), or known commands (`git`, `docker`), lean Shell.
   - Reference data: word list (similar to Warp's `natural_language_detection/words.txt`) + a baseline corpus.
2. **Neural** — BERT-tiny ONNX model, runs on-device.
   - Backend choice: **Candle** (pure Rust, easier deploy) or **ONNX Runtime via `ort` crate** (faster but bigger dep).
   - Sub-millisecond inference. No network call.
   - Train initial model on (Stack Overflow questions → AI) + (man-page command examples → Shell). Public corpora.

Both classifiers fall through: heuristic gives a fast first guess; neural disambiguates ambiguous cases. Identical to Warp's setup.

### b) `tender-completer-parser` — HIR-style command parser

Parse user input into a typed intermediate representation, not a token stream. Modelled on Warp's `warp_completer/parsers/hir/`:

```rust
pub struct ParsedTokenData {
    pub token: Token,
    pub token_index: usize,
    pub token_description: TokenDescription,
}

pub enum SuggestionType {
    Command,         // root command (e.g. "git")
    Subcommand,      // (e.g. "checkout" after "git")
    Flag,            // (e.g. "-b" after "git checkout")
    Argument,        // positional argument
    Filename,        // file/dir path
    Variable,        // shell variable / env var
}
```

Parser knows where you are in the command tree, so completions can be **semantic** ("after `git checkout -b`, suggest a branch name") rather than substring matches.

### c) `tender-completer-signatures` — the command knowledge base

This is the leverage. Two strategies, pick one:

**Strategy 1: Vendor Fig specs (recommended)**

- Fig autocomplete specs are MIT-licensed, ~5000 community-maintained TypeScript files at [withfig/autocomplete](https://github.com/withfig/autocomplete).
- Mirror Warp's pattern: TypeScript source compiled at build time, embedded into Rust binary via `rust-embed`.
- Build flow: `build.rs` → `yarn build` (in `js/`) → static JSON output → `rust-embed`.
- Pro: instant coverage of ~5000 commands, community keeps it fresh.
- Con: TypeScript + Node toolchain in the build (matches what Warp does).

**Strategy 2: Write signatures in Rust directly**

- Define a `CommandSignature` Rust type. Hand-author signatures for the top 50 commands.
- Pro: no TS toolchain.
- Con: 5000× less coverage. You'll write completions in Rust forever and never catch up.

Recommend Strategy 1. The Node build dep is a one-time cost; the spec ecosystem is the multiplier.

### d) `tender-completer-completer` — the assembly

Given parsed tokens + a signature + history + context, rank suggestions:

```rust
pub trait Completer {
    fn suggest(&self,
        buffer: &str,
        cursor: usize,
        ctx: &CompletionContext)
        -> Vec<Suggestion>;
}

pub struct CompletionContext {
    pub cwd:          PathBuf,
    pub git:          Option<GitContext>,         // branch, status hints
    pub history:      Vec<HistoryEntry>,          // recent commands (from Tender's event stream)
    pub env:          HashMap<String, String>,
    pub last_blocks:  Vec<BlockRef>,              // recent Tender blocks for cross-reference
}
```

Sources (in priority order, deduped on rendering):
1. **Signatures**: known flags/subcommands/positions for the current token.
2. **History**: prior invocations matching prefix (from Tender's event stream — see [event-emit-primitive](../completed/2026-07-07-event-emit-primitive.md)).
3. **Files / paths**: filesystem completion for positional file args.
4. **AI fallback**: when classifier says `AI`, or when no shell suggestions matched.

## Wiring into Tender's world

```
┌─────────────────────────────────────────────────────────────┐
│  tender-blocks-egui  (or any other UI)                       │
│    prompt input box ───── as user types ─────┐               │
└──────────────────────────────────────────────│───────────────┘
                                               │
                                               ▼
┌─────────────────────────────────────────────────────────────┐
│  tender-completer  (this crate)                              │
│    ① classifier   →  Shell | AI                              │
│    ② parser       →  ParsedTokenData[]                       │
│    ③ signatures   →  matched CommandSignature                │
│    ④ completer    →  Vec<Suggestion>                         │
└────────────┬────────────────────────────────────────────────┘
             │ history queries
             │ recent blocks
             ▼
┌─────────────────────────────────────────────────────────────┐
│  tender  (event stream + block store)                        │
│    tender watch  →  recent commands for history-based hints  │
│    tender list   →  recent blocks for cross-reference        │
└─────────────────────────────────────────────────────────────┘
```

Tender's role is **context**, not completion. The completer queries Tender for what's been run recently (to bias suggestions) but Tender doesn't know about completion at all.

## Scope

- New workspace member `tender-completer/` with four sub-crates (or one crate with four modules — decide at v1).
- Bindings to Fig autocomplete specs (vendored as a git submodule or fork).
- BERT-tiny ONNX model embedded via `rust-embed` (initial model is fine to be Warp-compatible if license permits; otherwise train one on the same Stack Overflow + man-page corpus).
- Public library API consumable by `tender-blocks-egui` and other UIs.
- Standalone `tender-complete` debug binary that takes stdin → emits classifications + suggestions as JSON (for testing and other tools to integrate).

## Non-goals

- **Not a shell.** Doesn't execute anything. Pure suggestion engine.
- **Not in core Tender.** Lives in separate workspace member; Tender does not depend on it.
- **No AI implementation here** — when classifier returns `AI`, dispatch to a separate consumer (whatever AI integration the UI provides). This crate decides "this is AI"; it does NOT call an LLM.
- **No theming / rendering** — returns structured `Suggestion` records; the UI renders them.
- **No prompt prediction** — just current-input completion, not "what would you type next."

## License — verified, settled

Earlier drafts of this plan suggested possibly lifting Warp's Rust code. **That is not viable.** Verified 2026-05-23 via per-crate license inspection:

| Component | License | Usable in Tender? |
|---|---|---|
| Warp workspace default | **AGPL-3.0-only** | ❌ Viral copyleft — would force Tender to AGPL |
| `warp_completer`, `input_classifier`, `command-signatures-v2`, `natural_language_detection`, `ai`, `mcp` | inherit AGPL | ❌ Cannot lift |
| `warpui`, `warpui_core` | MIT (overrides) | ✅ But we use egui instead |
| Fig autocomplete specs ([withfig/autocomplete](https://github.com/withfig/autocomplete)) | MIT, independent of Warp | ✅ |
| egui, eframe | MIT OR Apache-2.0 | ✅ |
| libghostty | MIT | ✅ |
| Candle / ort / ndarray / tokenizers | MIT/Apache-2.0 | ✅ |

**Implementation strategy**: Read Warp's open source as documentation. Write our own clean-room Rust implementation. Vendor Fig specs directly (independently MIT). Train our own ONNX classifier model. Tender's license stays `MIT OR Apache-2.0`.

This is more work than lifting Warp's code, but it's the only legal path that keeps Tender adoptable as a library by proprietary tools — which is the entire point of Tender's permissive license.

## Open questions

1. **Fig specs: vendor, submodule, or runtime fetch?** Vendor for predictable builds; submodule for easy updates; runtime fetch for hot-iteration. Recommend submodule with pinned commit.
3. **ONNX backend.** Candle is more Rust-native and lighter; `ort` is faster but pulls in C++. For a completer running on every keystroke, latency matters → may favour `ort`. Bench both.
4. **Where does history come from?** Three options:
   - From the shell's own history file (`~/.bash_history`, `~/.zsh_history`) — universal but messy.
   - From Tender's event stream — clean structured data; only covers Tender-supervised runs.
   - Both, with Tender preferred when present.
   Recommend "both with Tender preferred."
5. **Async API or sync?** Suggestions need to be fast (sub-10ms). Probably keep the hot path sync; async for slow sources (filesystem completion across SSH, etc.).
6. **Training data for our own classifier model.** Stack Overflow Data Dumps + GitHub commands extracted from CI scripts + `man` pages → command-flavoured. Plus AI conversation corpora (publicly available). One-time effort.
7. **Multi-shell support.** zsh vs bash vs fish parse slightly differently (quoting, expansion). HIR parser handles 90% the same; edge cases need shell-specific rules. v1: bash/zsh shared parser. Fish later.
8. **Cross-host context.** When `tender --host` runs remotely, where does the completer look up files? Recommend: completer queries Tender for the remote block's host context; filesystem completion routes through Tender's remote file ops.

## Acceptance criteria

- `tender_completer::classify("git checkout -b feature")` → `InputType::Shell`
- `tender_completer::classify("how do I list big files in this dir")` → `InputType::AI`
- `tender_completer::suggest("git ch", 6, ctx)` returns `checkout`, `cherry-pick` with descriptions
- `tender_completer::suggest("docker ", 7, ctx)` returns top docker subcommands with descriptions
- `tender_completer::suggest("git checkout -b ", 16, ctx)` returns local branch names (from git context)
- Sub-10ms latency for `suggest` on a warm cache, p99
- Sub-1ms latency for `classify`
- Cross-platform (macOS, Linux, Windows)
- Standalone `tender-complete` debug CLI for golden-master testing
- Documented; example usage in `tender-blocks-egui` integration

## Why this is a high-leverage backlog item

Most "AI in the terminal" features today are bolt-ons that interrupt your shell workflow with a chat UI. The Warp model — a 2-state classifier that quietly routes between shell completion and AI conversation, with no mode switch — is genuinely better UX. It's also small in scope (four discrete sub-modules), buildable in weeks, and once shipped lives forever as a Rust library other tools consume.

Combined with `event-emit-primitive` (which gives the completer rich history) and `egui-block-terminal` (which is the canonical UI consumer), this is the third pillar of the layer-5 ecosystem on top of Tender.

## Depends On

- `event-emit-primitive` — completer pulls history/context from Tender's event stream.
