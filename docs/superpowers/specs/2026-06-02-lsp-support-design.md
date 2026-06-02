# LSP support — design & phased implementation plan

**Date:** 2026-06-02
**Status:** In progress — **Phase 1 complete** (lifecycle + document sync); Phases 2–7 planned.

This document is both the design for LSP support in nxvim **and** a phase-by-phase
implementation plan. Each phase below is written to be **handed off to a fresh
context window**: it states its prerequisites, the exact files it touches, the
protocol surface it adds, the tests that prove it, and a hard "done when" gate.
Read the *Design* half first (everything down to *Phases*), then execute the
phases in order — later phases assume the foundations of earlier ones.

The closest existing subsystem, and the template for most of this work, is the
**treesitter syntax worker** ([syntax-highlighting design](2026-06-01-syntax-highlighting-design.md)):
an out-of-editor capability that is *advisory*, *fully async*, and *can never
stall the editor*. LSP reuses that shape — with one deliberate divergence (it is
**in-process**, not a separate nxvim worker — see [Decision 1](#decision-1-in-process-client-crate-not-a-worker-process)).

---

## Goal

Make nxvim a usable LSP client: connect to real language servers
(rust-analyzer, pyright, gopls, lua-language-server, …) and surface their
intelligence — **diagnostics, go-to-definition, hover, completion, rename,
formatting, code actions** — while preserving nxvim's two non-negotiables:

1. **The editor never blocks on a language server.** A slow or hung server can
   never freeze keystroke→buffer→redraw. Every LSP request is fired and
   forgotten; its reply arrives later as an event that triggers a redraw or a UI
   update, exactly like `ts_highlights`.
2. **`nxvim-core` stays pure and synchronous.** No LSP types, no JSON, no async,
   no I/O leak into core. LSP lives in a new crate and in `nxvim-server`; core
   gains only small, pure helpers (position-encoding math) and a cursor-jump API.

Compatibility target, per [architecture.md](../../architecture.md) guiding
principle 2, is the **Lua `vim.lsp.*` / `vim.diagnostic.*` API surface** that
modern plugins drive — but that is the *last* phase. Earlier phases stand up the
machinery behind a small **built-in config** (a filetype→server-command table,
the analogue of `filetype_of`), so the feature is useful long before the full Lua
API exists.

---

## Guiding constraints (inherited from the architecture)

- **Client-server, always.** Language features are produced server-side and
  projected to the TUI through the existing `redraw` map and (new) request/reply
  notifications. The TUI stays a dumb renderer.
- **The `View` is core-owned and pure.** Like treesitter highlights, LSP overlays
  (diagnostics, hover, the completion menu) are **not** added to `nxvim-core`'s
  `View`. The server merges them into the `redraw` map it already builds by hand.
  Core's only `View`-adjacent change is exposing a cursor-jump for go-to.
- **Effects flow through queues / events.** Server→language-server commands are
  fire-and-forget; language-server→server replies are events the main loop
  `select!`s on. This is the `SyntaxClient` / `SyntaxEvent` pattern verbatim.
- **Pinned exact deps**, added under `[workspace.dependencies]` and pulled in
  with `<dep>.workspace = true`.
- **Black-box tests only.** No `#[test]` units. Everything is proven through a
  running server driven over RPC, against a **mock language server** fixture
  (below), the LSP analogue of the syntax tests' fixture grammar.

---

## Key design decisions

### Decision 1: in-process client crate, **not** a worker process

The treesitter worker is a separate OS process for one reason only: **grammars
are compiled C and can segfault the host**, which no in-process guard survives.
That rationale **does not apply to LSP**. Language servers are *already* separate
OS processes; nxvim talks to them over pipes. A crashing rust-analyzer just
closes a pipe — it cannot segfault nxvim. The LSP *client* code (JSON-RPC
framing, `lsp-types`, request correlation) is pure safe Rust and cannot crash the
editor.

Therefore the LSP client runs **inside the server's runtime** (as spawned async
tasks), and spawns language servers as its **direct children**. This matches
neovim (its LSP client is in-process) and avoids a pointless double translation
(an `nxvim --__lsp-worker` would only re-encode msgpack↔JSON for no isolation
benefit).

To keep the heavy protocol machinery out of the server crate proper, it lives in
a **new `nxvim-lsp` crate** that `nxvim-server` depends on as a normal crate edge
(unlike `nxvim-ts`, which is a *process* edge). `serde_json` and `lsp-types` are
reached only through `nxvim-lsp`.

> **Rejected alternative:** an `nxvim --__lsp-worker` process mirroring the
> treesitter worker. Rejected because LSP servers are already isolated, so the
> extra process buys nothing and adds a latency hop and a second protocol
> translation. Noted here because the symmetry with `nxvim-ts` is tempting and a
> future contributor will ask.

### Decision 2: `async-lsp` drives the JSON-RPC layer, inside the manager

LSP is JSON-RPC 2.0 with `Content-Length:` headers over the server's
stdin/stdout. `nxvim-lsp` uses [`async-lsp`](https://docs.rs/async-lsp) to own
that layer — framing, request/response **id correlation**, `$/cancelRequest`,
concurrency, and backpressure — with `lsp-types` for the message *types* (which
`async-lsp` re-exports). We do **not** hand-roll the transport.

The reasoning, having weighed both honestly:

- **`tower-lsp` is out** for the right reason: it is built around the
  `LanguageServer` trait you implement to *build a server*; there is no real
  client story.
- **`async-lsp` is a real client framework** (`MainLoop::new_client`, a
  `ServerSocket` you call methods on, built-in client-process-exit monitoring).
  It hands us, maintained and tested, exactly the fiddly correctness-critical
  bits we'd otherwise own by hand.

Why default to it rather than hand-roll:

1. **It deletes the error-prone half.** `Content-Length` framing + JSON-RPC id↔
   reply correlation + `$/cancelRequest` is the part most likely to harbor subtle
   bugs (partial reads, interleaved cancels, response races). Letting a
   maintained crate own it is strictly less code for us to get wrong.
2. **It composes cleanly with our constraints.** `async-lsp`'s `MainLoop` is
   `Send` and runs off the editor thread, so it lives *inside* `LspManager`'s
   tasks. The `!Send`, one-message-at-a-time editor thread never touches it: it
   only ever sees `LspCommand` out / `LspEvent` in over channels (Decision 3).
   async-lsp's `.await`-style ergonomics are used *inside the manager task*, where
   awaiting is fine, and the result is forwarded as an `LspEvent` — the editor
   never blocks.
3. **The manager/bridge is ours regardless.** Whichever transport we pick, we
   still own per-`(server, root)` child management, editor-side position
   conversion, and the generation-token stale-drop. Picking async-lsp shrinks
   what's left to exactly that bridge — the interesting part — instead of also a
   codec.

> **The hand-rolled alternative** — a ~100–150-line `Content-Length` + JSON-RPC
> layer over the child's stdio (in the spirit of `nxvim-rpc`), adding only
> `serde_json` instead of tower — stays the **sanctioned fallback** if
> `async-lsp`'s tower/`Service` programming model proves an awkward fit or its
> dependency surface becomes a problem. The editor-facing design
> (`LspCommand`/`LspEvent`) is identical either way, so this choice is internal to
> `nxvim-lsp` and reversible without touching `nxvim-server`. *(Note the precedent
> cuts the other way and is deliberately not invoked here: nxvim hand-rolls
> `nxvim-rpc` because msgpack values are self-delimiting — the framing is
> near-trivial — and the protocol is bespoke with no off-the-shelf fit. LSP is the
> opposite on both counts: real `Content-Length` framing, and a maintained client
> framework that fits.)*

### Decision 3: requests never block — reply-as-event with a generation token

`nxvim-server` processes one message at a time against `!Send` editor state. It
**cannot** hold a `oneshot` across the main loop awaiting an LSP reply without
freezing the editor. So LSP requests follow the `SyntaxEvent` model:

- The server sends a *command* to the `nxvim-lsp` manager (e.g.
  `LspCommand::Hover { buffer, position, token }`), then returns to the loop.
- The manager's per-server `async-lsp` `MainLoop` task `await`s the reply (id↔
  reply correlation is async-lsp's job). When it lands, the manager emits an
  `LspEvent::Hover { token, result }` on the event channel the main loop
  `select!`s on.
- The server matches the reply to its intent by the **`token`** it issued and
  acts (open the hover panel, jump the cursor, …).

A monotonic **generation token** per request kind makes stale replies harmless:
if the cursor moved since a hover was requested, the reply's token is older than
the current generation and is dropped (and optionally `$/cancelRequest` is sent).
This is the exact role `tick` plays for `ts_highlights`.

### Decision 4: position encoding negotiated to UTF-8, with a UTF-16 fallback

nxvim columns are **byte offsets**; LSP `Position.character` is, by default,
**UTF-16 code units**. Modern servers honor the client's
`general.positionEncodings` capability; we advertise `["utf-8", "utf-16"]`
(preferring `utf-8`, which makes the conversion a no-op for ASCII and a cheap
byte count otherwise) and store the **negotiated encoding per server** from the
`initialize` result. All byte↔character conversion is **pure column math** and
lives in `nxvim-core::unicode` (alongside `virtcol`) as encoding-agnostic helpers
(`byte_to_utf16` / `utf16_to_byte`, with UTF-8 being identity). The server picks
which to apply per the negotiated encoding. Core gains *no* LSP concepts — just
two more unicode helpers.

> Getting this wrong corrupts every position the instant a line contains a
> non-ASCII character. It is called out explicitly in every phase that crosses a
> position.

### Decision 5: reuse the edit journal for `didChange`

`Buffer` already records `BufferEdit` deltas (byte ranges + `(row, col)` points)
and a `changedtick`, drained by `take_edits()` — built for treesitter. LSP
incremental sync (`TextDocumentSyncKind.Incremental`) needs exactly a range +
replacement text per change, which is the same delta in LSP coordinates. The
server converts each `BufferEdit` to an LSP `TextDocumentContentChangeEvent` (or
sends full text when a server requests `Full` sync, or on `resync`). **No new
core machinery** — the journal is shared between the syntax and LSP syncs.

### Decision 6: built-in server config first; `vim.lsp.*` last

Auto-start servers from a small built-in **filetype→command** table (e.g.
`rust`→`rust-analyzer`, `python`→`pyright-langserver --stdio`,
`go`→`gopls`, `lua`→`lua-language-server`), gated on the binary existing on
`PATH`. Overridable for tests by an env var pointing at the mock server. This is
the `filetype_of` pattern and lets every feature phase land before the large
`vim.lsp.*` Lua surface (Phase 7).

### Decision 7: UI strategy given nxvim has no floats, pmenu, or sign column yet

nxvim today has **no floating windows, no popup menu, no sign column, no virtual
text**. LSP results must land somewhere that exists. The plan threads features by
what they need:

| Feature                | Surface used (MVP)                                                            |
| ---------------------- | ---------------------------------------------------------------------------- |
| Diagnostics            | **underline/undercurl** via the existing highlight-span path + the **message line** (diagnostic under cursor) + the **bottom panel** as a diagnostics list |
| Go-to definition       | **cursor jump** (open/switch buffer + set cursor) — no new UI               |
| References / symbols   | the **bottom panel** (already selectable & jump-on-`<CR>`)                  |
| Hover / signature help | the **bottom panel** (markdown rendered as plain lines)                      |
| Completion             | a **new popup-menu surface** — its own phase (Phase 5) builds the pmenu      |
| Rename / format / code action | **buffer edits** (reuse `Buffer::insert`/`remove`); pick lists in the panel |

Floating windows, a real sign column, and inline virtual text are **follow-ups**,
noted where they would improve a feature. The panel ([architecture.md → *The
message panel*](../../architecture.md)) is the workhorse: it is already
bottom-docked, scrollable, and `<CR>`-selectable with a Lua/RPC callback — ideal
for location lists and hover text without inventing float layout.

---

## Architecture

```
                        crate edge (nxvim-server depends on nxvim-lsp)
┌────────────┐  redraw (+diag/hover/pmenu)  ┌──────────────┐    LspCommand     ┌───────────────┐
│ nxvim-tui  │ ◀─────────────────────────── │ nxvim-server │ ───────────────▶  │  LspManager   │
│ (client)   │  ──────── nvim_input ──────▶ │  (editor)    │ ◀── LspEvent ──── │  (nxvim-lsp)  │
└────────────┘                              └──────────────┘   mpsc channels   └───────┬───────┘
   main thread          nxvim-rpc            its own thread                            │ spawns children,
                                                                                       │ JSON-RPC / stdio
                                                                            ┌──────────┴──────────┐
                                                                            ▼          ▼          ▼
                                                                      rust-analyzer  pyright    gopls
                                                                      (Content-Length + JSON-RPC 2.0)
```

The `LspManager` is the LSP analogue of `SyntaxClient`: a handle the server holds
plus background tasks. Unlike `SyntaxClient` it manages **N** child processes
(one per `(server, workspace-root)`), each driven by its own `async-lsp`
`MainLoop` task (which owns that server's framing + JSON-RPC id space), and it
handles **request/reply** (not just notifications). The manager bridges those
per-server `MainLoop`s to the single `LspCommand`/`LspEvent` channel pair the
editor thread sees.

### Crate layout

| crate            | new? | role                                                                                                   |
| ---------------- | ---- | ------------------------------------------------------------------------------------------------------ |
| `nxvim-lsp`      | new  | The LSP client: `LspManager` (spawn/supervise/route N language servers via per-server `async-lsp` `MainLoop`s), the `LspCommand`/`LspEvent` bridge, server lifecycle. Heavy deps (`async-lsp`, `lsp-types`, `serde_json`) live **here only**. |
| `nxvim-server`   | —    | Gains an `LspManager` field + a third `select!` arm for `LspEvent`s. Owns: filetype→server config, per-buffer document-sync bookkeeping (reusing the edit journal), byte↔LSP position conversion (via core), diagnostics cache → redraw, request tokens, and the per-feature UI wiring. **Depends on `nxvim-lsp`.** |
| `nxvim-core`     | —    | Two additive, pure changes: UTF-16/UTF-8 column helpers in `unicode.rs`, and a cursor-jump (`Editor::jump_to(path, line, col)` reusing the existing buffer-open path). Stays pure & synchronous. |
| `nxvim-tui`      | —    | Renders the new redraw payloads: diagnostic underlines (already has underline/undercurl styles), the diagnostics/hover/symbols panel (already exists), and — in Phase 5 — the **completion popup menu** widget. |
| `nxvim` (bin)    | —    | No worker re-invoke needed (LSP is in-process). In test/debug builds, a hidden `--__lsp-mock` mode provides the mock language server fixture (see *Testing*). |

Dependency direction stays one-way and acyclic: `nxvim-server → nxvim-lsp` is a
new edge; nothing depends back on the server.

---

## Protocol & module surface

### `nxvim-lsp` public surface (sketch)

```rust
/// Handle the server holds; cheap, drives all language servers.
pub struct LspManager { /* cmd_tx, lazily-spawned supervisor, ... */ }

impl LspManager {
    pub fn new() -> (LspManager, Receiver<LspEvent>);
    /// Ensure a server for (language, root, cmd) is started; idempotent.
    pub fn ensure_server(&mut self, key: ServerKey, spawn: ServerSpawn);
    /// Fire-and-forget notifications (document sync, cancel, …).
    pub fn notify(&self, key: ServerKey, note: LspNotify);
    /// Fire a request; its reply returns as an LspEvent carrying `token`.
    pub fn request(&self, key: ServerKey, token: ReqToken, req: LspRequest);
    pub fn shutdown(&self, key: ServerKey);
}

/// Server → editor, delivered to the main loop's select!.
pub enum LspEvent {
    Initialized { key: ServerKey, capabilities: ServerCaps, encoding: PositionEncoding },
    Diagnostics { uri: Url, version: Option<i32>, diagnostics: Vec<Diagnostic> },
    Reply       { token: ReqToken, result: LspReply },   // hover/definition/completion/…
    ServerExited { key: ServerKey, status: ExitStatus },
    Log         { key: ServerKey, message: String },     // window/logMessage, stderr
}
```

- **`LspCommand`/`LspNotify`/`LspRequest`** carry data already in LSP
  coordinates. The server does *all* byte↔position conversion before sending and
  after receiving, because only it has the buffer text + negotiated encoding. The
  manager just ferries.
- **`ReqToken`** = `(kind, generation)`; the server drops a `Reply` whose
  generation is stale (Decision 3). The manager translates a `request` into an
  `await` on the server's `async-lsp` `ServerSocket` *inside the `MainLoop`
  task*, then forwards the resolved value as an `LspEvent::Reply` — the editor
  never awaits.
- Lifecycle (`initialize`/`initialized`/`shutdown`/`exit`), framing, id
  correlation, and `$/cancelRequest` are owned by `async-lsp` within the manager;
  the server sees only the distilled events above.

### Server-side per-buffer state (in `nxvim-server`)

Mirrors `SyntaxState`, keyed by `BufferId`:

```rust
struct LspDocState {
    server: Option<ServerKey>,   // which server owns this buffer (None = unsupported ft)
    opened: bool,                // didOpen sent for current content?
    version: i32,                // LSP document version (monotonic, == didChange count)
    last_tick: u64,              // changedtick mirror, drives didChange (shares the journal)
    diagnostics: Vec<Diagnostic>,// latest publishDiagnostics for this buffer
}
```

Diagnostics are cached here and projected into the `redraw` map (a new
`diagnostics` key), the same way `SyntaxState::spans` becomes the `highlights`
key. The byte ranges are converted to screen columns with `unicode::virtcol`
(the exact path highlights/selection use).

### Core changes (pure, additive)

1. `nxvim-core/src/unicode.rs`:
   - `byte_to_utf16(line: &str, byte: usize) -> usize`
   - `utf16_to_byte(line: &str, u16_units: usize) -> usize`
   (UTF-8 is the identity on byte offsets; UTF-32 = char count, add if a server
   ever needs it.) Pure, table-free, fuzz-friendly.
2. `nxvim-core/src/editor.rs`:
   - `Editor::jump_to(path: &str, line: usize, col: usize)` — open-or-switch to
     the buffer for `path` (reuse the `:e` path used by `open_or_named`), set the
     cursor, record the jump in the alternate/jumplist as `:e` already does. Used
     by go-to-definition and panel location-list selection. Purely a composition
     of existing buffer-switch + cursor-set; no new state.

No other core changes across all phases.

---

## Testing strategy (black-box, per the no-unit-test rule)

Everything is exercised through the running server over RPC, against a **mock
language server**. The mock is the LSP analogue of the syntax tests' fixture
grammar:

- **Mock server fixture.** The `nxvim` binary, in debug builds, supports a hidden
  `--__lsp-mock <script>` mode: it speaks real LSP (Content-Length + JSON-RPC 2.0)
  over stdio and returns **scripted, deterministic** responses — a fixed
  capability set, canned diagnostics, a canned hover/definition/completion — and
  **records every notification it received** to a file the test can read back.
  The server's filetype→command table is overridden in tests via an env var
  (`NXVIM_LSP_CMD` / a per-language override) to launch this mock instead of a
  real server. This keeps tests hermetic and network-free, exactly like
  `NXVIM_TS_WORKER` / `NXVIM_DATA_DIR`.
- **Async polling.** LSP replies are asynchronous, so redraw/state assertions
  **poll** (bounded wait) until the expected payload arrives (diagnostics in a
  redraw, the hover panel opening, the cursor having jumped) — never a single
  barrier. This is the pattern the syntax tests already use.
- **Tiers:** RPC/`View` integration tests in a new
  `crates/nxvim-server/tests/lsp.rs` (the bulk); a Tier-2 screen test in
  `crates/nxvim/tests/` for the painted result of diagnostics underlines and the
  completion pmenu; the position-encoding helpers are covered indirectly through
  a non-ASCII-line test that asserts a diagnostic/hover lands on the right cells.
- **Resilience test:** a mock that exits/hangs on a request; assert the editor
  **still edits** (`i…<Esc>` changes the buffer) and the stale reply is dropped —
  the LSP analogue of the syntax crash test.

The wall-clock latency of replies is **not** asserted (async), matching the
coverage boundary the syntax and smooth-scrolling designs set.

---

## Dependencies (pinned `=x.y.z`, latest stable; pin exactly at implementation)

Added under `[workspace.dependencies]`, reached **only** through `nxvim-lsp`
(plus `tokio`'s `process` feature, already enabled for the syntax worker):

- `async-lsp = "=0.2.4"` (features: `tokio`; `client-monitor` for child-exit
  detection) — the JSON-RPC client `MainLoop`, framing, id correlation,
  `$/cancelRequest`, and concurrency. A client drives `MainLoop::run` over the
  spawned server's `child.stdout`/`child.stdin` (tokio pipes).
- `lsp-types = "=0.97.0"` — the LSP protocol types (`Diagnostic`,
  `CompletionItem`, `WorkspaceEdit`, `Position`, `Url`, …). `async-lsp` builds on
  this same crate; pin one version so the types match.
- `serde_json = "=1.0.150"` — JSON values where we touch them directly (server
  config, mock fixture); most (de)serialization is handled by `async-lsp`.
- (`serde`/`tower` come transitively; pin explicitly only if a crate names them.)

We do **not** add `tower-lsp` (server-only — see Decision 2). The hand-rolled
`Content-Length` + JSON-RPC layer (~100–150 lines, adding only `serde_json` in
place of `async-lsp`) remains the sanctioned fallback if `async-lsp`'s tower model
or dependency surface proves an awkward fit; it is internal to `nxvim-lsp` and
swappable without touching `nxvim-server`.

Verify the exact latest patch versions with `cargo search` at implementation
time and pin them, as the rest of the workspace does.

---

## Phases (the handoff plan)

Seven phases. Each ends with whole-workspace `cargo fmt --all -- --check`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace` green
(default features only — never `--all-features`, per CLAUDE.md). Each is sized to
one focused context window.

---

### Phase 1 — `nxvim-lsp` crate: lifecycle + document sync (foundation) — ✅ DONE

**Goal / value.** Stand up the whole LSP plumbing with **no user-visible feature
yet**: a buffer of a configured filetype auto-starts its language server, the
`initialize`/`initialized` handshake completes, the negotiated position encoding
is recorded, and `didOpen`/`didChange`/`didClose`/`didSave` flow correctly.
Proven entirely against the mock server, which records what it received.

> **Implementation notes (as built).** Two deviations from the plan above, both
> internal and reversible:
> - **`lsp-types` pinned to `=0.95.1`, not `=0.97.0`.** `async-lsp 0.2.4` builds
>   against `lsp-types 0.95.1`; pinning `0.97.0` would resolve a *second*,
>   incompatible `lsp-types` and fail to compile. `0.95.1` still uses
>   `url::Url` (the `Url`↔`Uri` rename landed in 0.97), so the design's `Url`
>   naming is correct as written. Bump both together if async-lsp updates.
> - **The `async-lsp` client service is the bare `Router`** (no `tower`
>   `ServiceBuilder`/concurrency/catch-unwind stack): the client only *receives*
>   notifications (diagnostics, log/show messages) whose handlers are trivial and
>   panic-free, so no `tower` dependency was added. tokio child pipes are bridged
>   to async-lsp's `futures`-io `MainLoop` with `tokio-util`'s `compat`.
> - **Added deps** (under `[workspace.dependencies]`): `async-lsp =0.2.4`
>   (`default-features = false`, features `omni-trait`, `tokio`), `lsp-types
>   =0.95.1`, `serde_json =1.0.150`, `tokio-util =0.7.18` (feature `compat`).
> - **`didSave` is detected heuristically** server-side (`modified` cleared with
>   no `changedtick` change ⇒ a `:w`, distinguished from undo-to-clean which bumps
>   the tick), since core exposes no save event and gains none (Decision 5 / "no
>   other core changes"). A dedicated save hook is a possible later refinement.
> - **Tests live in `crates/nxvim/tests/lsp.rs`** (not `nxvim-server/tests/`):
>   spawning the `nxvim --__lsp-mock` binary needs `CARGO_BIN_EXE_nxvim`, which is
>   only set for the `nxvim` crate's integration tests — exactly where the syntax
>   worker tests live, for the same reason.
> - **Workspace root** is currently the file's parent directory; root-marker
>   search (`Cargo.toml`/`.git`) is a later refinement.

**Prerequisites.** None.

**Scope (in):**
- New `nxvim-lsp` crate: wire `async-lsp`'s client `MainLoop` over a spawned
  child's stdio (one `MainLoop` task per server), and the `LspManager` that
  spawns/supervises/respawns them + bridges to a single event channel (model the
  supervision/backoff/circuit-breaker on `SyntaxClient`'s
  `supervise`/`run_worker_once`, for a server that won't start or crash-loops).
  async-lsp owns framing + id correlation + `$/cancelRequest`; the manager owns
  the child lifecycle, the breaker, and the `LspCommand`/`LspEvent` translation.
- Lifecycle: `initialize` (advertise `positionEncodings: ["utf-8","utf-16"]` and
  the document-sync client capabilities), handle the result (capabilities +
  chosen encoding), send `initialized`; clean `shutdown`/`exit` on buffer/last
  close and server teardown.
- The `LspCommand`/`LspEvent` enums (Decision 3), `ServerKey`,
  `PositionEncoding`.
- `nxvim-core`: the two `unicode.rs` position helpers (Decision 4). Cover them
  via the integration tests below (non-ASCII line).
- `nxvim-server`: the filetype→command built-in table + `NXVIM_LSP_CMD` test
  override; `LspDocState` per buffer; the third `select!` arm draining
  `LspEvent`s (initially handling only `Initialized`/`ServerExited`/`Log`);
  document sync driven from `redraw()`/the input path, reusing `take_edits()` and
  `changedtick` (full sync vs incremental per the server's reported
  `TextDocumentSyncKind`; `resync` → re-`didOpen` with full text + bumped
  version); `didClose` on `:bdelete` (reuse the `reap_closed_buffers` hook).
- The mock server fixture (`nxvim --__lsp-mock`) + the test harness override.

**Scope (out → later phases):** diagnostics rendering (Phase 2) — but if the
mock sends `publishDiagnostics`, just cache them; any language *feature* request;
all UI.

**Files.**
- `crates/nxvim-lsp/{Cargo.toml, src/lib.rs, src/manager.rs}` (new; the
  `MainLoop` wiring + `LspCommand`/`LspEvent` bridge. The fallback hand-rolled
  transport, if ever needed, would add `src/codec.rs`/`src/jsonrpc.rs` here.)
- `crates/nxvim-core/src/unicode.rs` (add helpers)
- `crates/nxvim-server/src/lsp.rs` (new: config table, `LspDocState`, sync logic, event handling — the `syntax.rs` analogue)
- `crates/nxvim-server/src/lib.rs` (wire the manager field, the `select!` arm, drain hooks)
- `crates/nxvim/src/main.rs` (+ `--__lsp-mock` debug mode), `crates/nxvim/Cargo.toml`
- `Cargo.toml` (workspace member + deps)
- `crates/nxvim-server/tests/lsp.rs` (new)

**Tests (black-box).**
- Open a `.rs` buffer (mock launched via override); poll until the mock's record
  file shows `initialize` (with `utf-8` advertised) and a `didOpen` whose `text`
  equals the buffer contents and whose `languageId` is `rust`.
- Type `ihello<Esc>`; assert the mock received a `didChange` with the right
  version bump and a content change matching the edit (incremental range +
  `"hello"`).
- **Position encoding:** put a non-ASCII char before an edit; assert the
  `didChange` range's `character` is the correct UTF-8 (or negotiated) unit, not
  the byte (the regression guard for Decision 4).
- `:w` → `didSave`; `:bd` → `didClose`. Editing a `.txt`/no-path buffer starts
  **no** server.
- **Resilience:** a mock that exits right after `initialize`; assert the editor
  still edits and the manager records the exit (and respawns per backoff, or
  gives up cleanly past the breaker).

**Done when.** All of the above pass; `nxvim-core` still has no async/LSP/JSON
deps; the three workspace gates are green.

---

### Phase 2 — Diagnostics

**Goal / value.** The first visible payoff: squiggles + messages. Handle
`textDocument/publishDiagnostics`, cache per buffer, and render them three ways:
**underline/undercurl** spans (severity-colored), the **diagnostic under the
cursor** on the message line, and a **diagnostics list** in the bottom panel.

**Prerequisites.** Phase 1.

**Scope (in):**
- `LspEvent::Diagnostics` → store in `LspDocState.diagnostics` (route by `uri` →
  `BufferId`; drop unknown/closed buffers, as `store_spans` does); mark a redraw
  dirty (the `syntax_dirty` pattern).
- New `redraw` key **`diagnostics`**: per-visible-row spans
  `[start_col, end_col, severity, style_id]` in screen columns (convert byte→
  screen with `virtcol`; convert LSP char→byte first via the negotiated
  encoding). Severity → a style via highlight groups
  `DiagnosticUnderlineError/Warn/Info/Hint` (resolve through the existing
  registry/`StyleTable`, same as chrome).
- Message line: when the cursor sits on a diagnostic, show its message (highest
  severity wins) via the existing `echo`/message path.
- `:LspDiagnostics` (and/or a keymap) → open the **panel** as a location list of
  all diagnostics for the buffer; `<CR>` jumps via `Editor::jump_to` (Phase-1
  core add — pull it forward if not yet present, it's tiny).
- TUI: paint the `diagnostics` underline spans, composing over syntax/selection
  (syntax sets `fg`; diagnostic adds `UNDERLINED`/`UNDERCURL` + `sp`). Localized
  to `highlight_line`, like the selection composition.

**Scope (out):** a real sign column and inline virtual text (follow-ups —
underline + message line + panel cover the MVP); `vim.diagnostic.*` Lua API
(Phase 7).

**Files.** `crates/nxvim-server/src/lsp.rs`, `crates/nxvim-server/src/lib.rs`
(redraw key + projection, mirroring `highlights_for`), `crates/nxvim-tui/src/render.rs`,
`crates/nxvim-server/tests/lsp.rs`, a Tier-2 screen test in `crates/nxvim/tests/`.

**Tests.**
- Mock pushes diagnostics for known ranges; poll a redraw until `diagnostics`
  appears; assert spans, severities, and **screen columns** (include a leading
  tab + a non-ASCII line to prove byte→screen and char→byte conversion).
- Cursor on the diagnostic → message line shows its text; off it → cleared.
- Panel lists all diagnostics; `<CR>` jumps the cursor to the right line/col.
- Tier-2 screen: a diagnostic cell is painted underlined with the error `sp`
  color, and still carries its syntax `fg`.

**Done when.** The above pass; gates green.

---

### Phase 3 — Go-to definition & references

**Goal / value.** Navigation: `textDocument/definition` (+ `declaration`,
`typeDefinition`, `implementation`) jumps the cursor; `textDocument/references`
fills the panel as a jump list. Cheapest high-value features — no new UI.

**Prerequisites.** Phase 1 (and `Editor::jump_to`; Phase 2 if not pulled forward).

**Scope (in):**
- Issue the request with a `ReqToken` (Decision 3) carrying the cursor position
  converted to LSP coords; on `LspEvent::Reply` with a matching, current token,
  act:
  - **definition-family:** single `Location`/`LocationLink` → `Editor::jump_to`;
    multiple → panel location list.
  - **references:** always a panel location list; `<CR>` jumps.
- Keymaps `gd`/`gD`/`gr` (and ex-commands `:LspDefinition`/`:LspReferences`) as a
  built-in default (revisit once `vim.keymap` exists in Phase 7).
- Stale-reply drop: moving the cursor before the reply arrives invalidates the
  token (and optionally `$/cancelRequest`).

**Scope (out):** the jumplist UI beyond what `jump_to` records; workspace symbol
search (a later/optional add).

**Files.** `crates/nxvim-server/src/lsp.rs`, `crates/nxvim-server/src/lib.rs`
(command routing, token generations, keymap/ex-command wiring),
`crates/nxvim-server/tests/lsp.rs`.

**Tests.**
- Mock returns a definition `Location` in the same and in a *different* file;
  assert `gd` switches buffer (when needed) and lands the cursor at the right
  (line, col), with non-ASCII columns converted correctly.
- Multiple definitions / references populate the panel; `<CR>` jumps.
- Fire `gd`, move the cursor, then deliver a stale reply → no jump (token drop).

**Done when.** The above pass; gates green.

---

### Phase 4 — Hover & signature help

**Goal / value.** `textDocument/hover` on `K` shows docs in the panel;
`textDocument/signatureHelp` shows the active signature (panel or message line).

**Prerequisites.** Phase 1, Phase 3 (token/Reply infra).

**Scope (in):**
- Hover: request with a token at the cursor; on reply, render the
  `MarkupContent`/`MarkedString` as **plain lines** in the bottom panel (strip or
  lightly render markdown — full markdown rendering is a follow-up). Empty hover →
  a brief "no information" message, not an empty panel.
- Signature help: render the active signature + active parameter in the panel or
  on the message line; triggered manually (e.g. `<C-k>` in insert) — auto-trigger
  on `(`/`,` is deferred to keep insert-mode untouched until completion (Phase 5).
- Reuse the stale-token drop.

**Scope (out):** floating windows (the natural home — a follow-up once floats
exist); markdown styling beyond plain text.

**Files.** `crates/nxvim-server/src/lsp.rs`, `crates/nxvim-server/src/lib.rs`,
`crates/nxvim-server/tests/lsp.rs`.

**Tests.**
- Mock returns hover markup; `K` opens the panel with the expected lines.
- Empty hover → message, no panel.
- Signature help renders the active parameter.

**Done when.** The above pass; gates green.

---

### Phase 5 — Completion (the popup menu)

**Goal / value.** The big UI lift and the headline feature:
`textDocument/completion` driven from insert mode, shown in a **new popup-menu
(pmenu) surface**, accepting an item inserts its text (+ `additionalTextEdits`).

**Prerequisites.** Phases 1–3 (sync, tokens, edit application groundwork).

**Scope (in):**
- A **pmenu surface**: a new redraw region (`pmenu` map: items, selected index,
  anchor screen position, dimensions) the server projects, and a **ratatui pmenu
  widget** in the TUI that floats over the text area at the anchor (this is the
  first overlay widget; build it minimally — a bordered list with a selected
  row). Core stays out of it: the server owns the menu model and drives it from
  insert-mode state, exactly as it owns diagnostics.
- Trigger: manual `<C-x><C-o>`/`<C-Space>` first; auto-trigger on typing is a
  follow-up toggle once the manual path is solid.
- Request completion at the cursor (token); on reply, populate the pmenu
  (respect `isIncomplete`, `CompletionItemKind` for an icon/label, `filterText`).
  Navigate with `<C-n>`/`<C-p>`/arrows; `<CR>`/`<Tab>` accepts → apply the item's
  `textEdit` (or insert `insertText`/`label`) + any `additionalTextEdits` via
  `Buffer::insert`/`remove`; `<Esc>`/`<C-e>` cancels.
- `completionItem/resolve` for lazy docs/detail (optional within this phase).

**Scope (out):** snippets (`InsertTextFormat.Snippet` placeholder expansion) —
insert the plain text for now, snippet expansion is a follow-up; fuzzy ranking
beyond the server's order + prefix filter.

**Files.** `crates/nxvim-core/src/view.rs` *only if* the pmenu anchor needs a
core-computed cursor screen position already available (it does:
`cursor_screen_col` exists — so likely **no** core change), `crates/nxvim-server/src/lsp.rs`,
`crates/nxvim-server/src/lib.rs` (pmenu redraw key + insert-mode menu state
machine), `crates/nxvim-tui/src/{render.rs, lib.rs}` (pmenu widget + key
handling while the menu is open), `crates/nxvim-server/tests/lsp.rs`, a Tier-2
screen test in `crates/nxvim/tests/`.

**Tests.**
- Trigger completion; mock returns items; poll a redraw until `pmenu` appears
  with the expected items and selection.
- `<C-n>`/`<C-p>` move selection; `<CR>` inserts the accepted item's text
  (assert buffer contents) including an `additionalTextEdits` (e.g. an
  auto-import line); `<Esc>` dismisses without inserting.
- Tier-2 screen: the pmenu is painted over the text at the cursor anchor with the
  selected row highlighted.
- Non-ASCII line: the inserted edit lands at the right byte offset (encoding).

**Done when.** The above pass; gates green. *(This phase is the largest; if it
overflows a context, split at the pmenu-widget boundary: 5a = surface + widget +
manual navigation with mock items, 5b = real completion request + accept/edit
application.)*

---

### Phase 6 — Edits: formatting, rename, code actions

**Goal / value.** Buffer-mutating features: `textDocument/formatting` (and
range/onType later), `textDocument/rename` (apply a `WorkspaceEdit` across
buffers), `textDocument/codeAction` (list in the panel, apply on select).

**Prerequisites.** Phases 1–3 (sync, tokens, `jump_to`); the edit-application
helper from Phase 5 generalizes here.

**Scope (in):**
- A shared **`WorkspaceEdit`/`TextEdit[]` applier**: convert LSP ranges → byte
  ranges (encoding-aware), apply via `Buffer::insert`/`remove` in reverse order
  per document so offsets stay valid; touch every affected (open) buffer; bump
  versions so the next `didChange` is consistent. Resync the syntax/LSP shadow
  after a bulk apply (the journal already carries it).
- `:LspFormat` (+ an optional `format_on_save` flag wired into the `:w` path).
- `:LspRename {newname}` → request → apply the returned `WorkspaceEdit`.
- `textDocument/codeAction` at cursor/selection → panel list → apply the chosen
  action's edit (and/or run its `command` if it's server-resolved).

**Scope (out):** workspace-wide edits to **unopened** files (open-then-edit, or a
follow-up that writes them directly); `codeAction` commands that need
`workspace/executeCommand` round-trips beyond a single reply (follow-up).

**Files.** `crates/nxvim-server/src/lsp.rs` (the applier + the three features),
`crates/nxvim-server/src/lib.rs` (ex-commands, `:w` hook for format-on-save),
`crates/nxvim-server/tests/lsp.rs`.

**Tests.**
- Mock returns formatting `TextEdit`s; `:LspFormat` rewrites the buffer to the
  expected contents (assert lines); idempotent on re-run.
- Rename returns a multi-file `WorkspaceEdit`; assert each affected open buffer
  changed correctly; cursor/marks survive.
- Code action list populates the panel; selecting applies the edit.
- Encoding: an edit on a non-ASCII line lands at the right bytes.

**Done when.** The above pass; gates green.

---

### Phase 7 — `vim.lsp.*` / `vim.diagnostic.*` Lua API & config

**Goal / value.** Make it plugin-compatible: replace the built-in filetype→cmd
table with the Lua surface real configs drive, so a user's `init.lua` (or an
lspconfig-style setup) starts and configures servers. This is what turns the
machinery from "nxvim-native LSP" into "runs the ecosystem's LSP config."

**Prerequisites.** Phases 1–6 (the full feature set behind the native config).

**Scope (in):** the minimal but real surface modern configs touch, layered on the
`nxvim-lua` queue/effect pattern (`vim.cmd`/`nvim_set_hl` style — Lua queues
ops, the server drains them into the `LspManager`):
- `vim.lsp.start(config)` / `vim.lsp.config(name, opts)` / `vim.lsp.enable(name)`
  / `vim.lsp.buf_attach_client` — start/attach servers from Lua, replacing the
  built-in table (which becomes a fallback/example).
- `vim.lsp.buf.*` (`definition`, `references`, `hover`, `rename`, `format`,
  `code_action`, `completion`) — Lua entry points to the Phase 3–6 features, so
  user keymaps can call them (needs `vim.keymap.set`, which this phase adds if not
  already present — coordinate with the separate keymap roadmap item).
- `vim.diagnostic.*` (`get`, `setloclist`/panel, `goto_next`/`goto_prev`,
  `config` for display toggles) over the Phase-2 diagnostics cache.
- Surface server capabilities/notifications Lua needs (`on_attach` callback,
  `client.server_capabilities`).

**Scope (out):** the *entire* `vim.lsp` surface (it's vast) — implement what
common configs require and grow it as plugins demand, per architecture.md's
"surface grows only as plugins demand it." Legacy Vimscript configs are a non-goal.

**Files.** `crates/nxvim-lua/src/lib.rs` + `prelude.lua` (the `vim.lsp`/
`vim.diagnostic` tables, queued ops), `crates/nxvim-server/src/lsp.rs` /
`lib.rs` (drain Lua LSP ops; route `vim.lsp.buf.*` to the existing feature paths;
fire `LspAttach`/`LspDetach` autocmds), `crates/nxvim-server/tests/lsp.rs`.

**Tests.**
- An `init.lua` calling `vim.lsp.start{...}` (pointed at the mock) attaches and
  produces diagnostics/hover/definition through the *Lua* path (not the built-in
  table).
- `vim.diagnostic.get`/`goto_next` behave against canned diagnostics.
- A Lua keymap calling `vim.lsp.buf.definition()` jumps the cursor.

**Done when.** A realistic Lua-driven setup against the mock exercises
diagnostics + go-to + hover + completion + format end-to-end; gates green.

---

## Cross-phase notes & follow-ups (not scheduled)

- **Floating windows** — the proper home for hover, signature help, and the
  pmenu border. The plan uses the panel/a minimal overlay until floats exist;
  building real float layout is a separate roadmap item that would upgrade
  Phases 4–5.
- **Sign column & inline virtual text** — upgrade diagnostics (Phase 2) once a
  gutter-sign and virt-text surface exist.
- **Snippets** (`InsertTextFormat.Snippet`) — upgrade completion accept (Phase 5).
- **Multiple windows** — go-to and diagnostics assume one window onto one
  buffer; revisit when splits land (an open roadmap item).
- **Workspace features** — `workspace/symbol`, `workspace/didChangeWatchedFiles`,
  `workspace/executeCommand` beyond single-reply actions: add as needed.
- **Pull diagnostics** (`textDocument/diagnostic`) — the plan uses push
  (`publishDiagnostics`); add the pull model if a target server requires it.

## Compared to neovim

- **In-process LSP client**, like neovim — but spawning servers from a Rust
  manager with reply-as-event correlation instead of Lua coroutines, because the
  server loop is single-message-at-a-time and must never block (Decision 3).
- **Reuses the treesitter edit journal** for `didChange` — neovim tracks LSP
  document changes separately; nxvim already has the deltas.
- **Panel-first UI** — neovim leans on floats from day one; nxvim defers floats
  and routes hover/symbols/lists through its existing message panel until a float
  surface exists.
- **`vim.lsp.*` last** — the machinery is native first (built-in config), the Lua
  surface is layered on top, matching how nxvim grew `nvim_set_hl`/`:colorscheme`
  before a broad `vim.*`.
</content>
</invoke>
