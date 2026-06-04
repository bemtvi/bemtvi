# LSP support — design & phased implementation plan

**Date:** 2026-06-02
**Status:** In progress — **Phases 1–6 complete** (lifecycle + document sync; diagnostics; go-to definition & references; hover & signature help; completion — the popup menu, ordered & live-refreshing; edits — formatting, rename across open buffers, code actions); Phase 7 (`vim.lsp.*` Lua surface) planned.

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
sends full text when a server requests `Full` sync, or on `resync`).

> **Corrected during Phase 5 (2026-06-03).** The original plan said the *same*
> journal is shared between the syntax and LSP syncs with **no new core
> machinery**. That is wrong: `take_edits()` is **destructive**, and the syntax
> sync runs first, so once the worker is caught up (not mid-parse) it drained the
> journal before the LSP sync ran — every `didChange` then carried **0 changes**
> and the language server's document froze at `didOpen` (completion, hover, …
> answered against stale text). The fix is a **second, parallel journal**:
> `Buffer` records each edit into both, and the LSP sync drains its own via
> `take_lsp_edits()`, independent of the worker's drain rate. (Plus: a request
> first flushes pending changes via `sync_lsp`, since requests fire during input —
> ahead of `redraw`'s sync — so the server never answers a document older than the
> cursor the request was issued at.)

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
> - **`didSave` uses a real save hook**, not a heuristic: `Buffer` gained a
>   monotonic `save_tick` counter (the save analogue of `changedtick`), bumped
>   only on a successful `Buffer::write`. The server mirrors it per buffer in
>   `LspDocState` and fires `didSave` exactly when it advances — so undo-back-to-
>   the-saved-state (which clears `modified` without a `:w`) is never mistaken for
>   a save. This is a small, pure, editor-domain core addition (no I/O, no LSP
>   concepts), and a future `BufWritePost` autocmd can read the same signal.
> - **Tests live in `crates/nxvim/tests/lsp.rs`** (not `nxvim-server/tests/`):
>   spawning the `nxvim --__lsp-mock` binary needs `CARGO_BIN_EXE_nxvim`, which is
>   only set for the `nxvim` crate's integration tests — exactly where the syntax
>   worker tests live, for the same reason.
> - **Workspace root** (refined during Phase 6): `$NXVIM_LSP_ROOT` overrides it
>   explicitly; otherwise the nearest ancestor holding a language root marker
>   (`Cargo.toml` for rust, `go.mod` for go, `pyproject.toml`/… for python, …)
>   then any `.git` ancestor, falling back to the file's parent directory. The
>   resolved root is cached per file path so the marker walk doesn't run on every
>   redraw. (Originally the bare parent directory.)
> - **Observability (added for following along before any on-screen feature
>   exists):** an `:LspInfo` ex-command opens the panel with the current buffer's
>   server / encoding / sync-kind / version / cached-diagnostics count plus the
>   list of running servers; and an append-only LSP log at
>   `$XDG_STATE_HOME/nxvim/lsp.log` (else `~/.local/state/nxvim/lsp.log`) in the
>   `[LEVEL][UTC ts] server\tmessage` shape, capturing lifecycle, server
>   `window/logMessage`+`showMessage` at their mapped severity, captured server
>   **stderr**, and (at DEBUG) outgoing `did*` sync traffic. Level is set by
>   `$NXVIM_LSP_LOG_LEVEL` (`off`/`error`/`warn`/`info`/`debug`/`trace`, default
>   `warn`); `$NXVIM_LSP_LOG_FILE` overrides the path. `window/logMessage` goes to
>   the log only; `window/showMessage` (user-facing) also reaches `:messages`.

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

### Phase 2 — Diagnostics — ✅ DONE

> **Implementation notes (as built).** Faithful to the plan, with a few choices
> worth recording:
> - **`Editor::jump_to(path, line, col)` was pulled forward** (as the plan
>   permits) and lives in `nxvim-core::editor` — a pure composition of the
>   existing `:e` open-or-switch path and the search-landing cursor-set, taking a
>   **byte** column (the server converts the LSP encoding first). It records the
>   alternate `#` like `:e` and never reloads-in-place / guards `modified` (a jump
>   navigates, it doesn't discard edits). Phase 3 reuses it for go-to.
> - **Message line is injected at projection time, not via `echo`.** The
>   under-cursor diagnostic (`diagnostic_under_cursor`, highest severity wins) is
>   written into the redraw's `message` field **only when the editor's own
>   message line is empty**, so it never pollutes `:messages` on every cursor
>   move and never clobbers a real error/command message. It is recomputed each
>   redraw from the cache + live cursor, so it is always current and self-clears.
> - **`diagnostics` redraw key** mirrors `highlights`: per visible row, spans
>   `[start_col, end_col, severity, style_id]` in screen columns, aligned with
>   `numbers`. Severity is `1`=error … `4`=hint; `style_id` indexes the frame
>   palette when `DiagnosticUnderline{Error,Warn,Info,Hint}` resolves through the
>   registry, else `Nil`. A zero-width range is widened to one cell so it shows.
> - **TUI composition** adds the underline **last** in `cell_style` (after syntax,
>   search, and selection), so a diagnostic cell keeps its syntax fg + selection
>   bg and only gains the `sp` underline color + undercurl/underline modifier
>   (themed), or a built-in severity color (red/yellow/cyan/grey) with no theme.
>   The slide band carries no diagnostics (they reappear when the scroll settles),
>   matching how search spans are handled.
> - **`:LspDiagnostics`** opens a **navigable** panel (`severity  line:col
>   message`, sorted by position) whose per-line jump targets are attached via
>   `Editor::set_panel_targets`; a `<CR>` on a target line `jump_to`s it and
>   closes the panel. *(Originally a server-side `lsp_panel_locations` list keyed
>   by panel-select index; that was replaced — see the Phase-3 note — by making
>   the panel itself navigable in the core, so the targets travel with the
>   `:panelopen` snapshot and can't drift from the panel they belong to.)*
> - **Mock** gained a `diagnostics` script field; it pushes
>   `textDocument/publishDiagnostics` for a document the instant it sees that
>   document's `didOpen`. Tests (in `crates/nxvim/tests/lsp.rs`, reusing the
>   Phase-1 mock harness) cover screen-column conversion (leading tab + 2-byte
>   `é`), the under-cursor message line on/off, the panel list + `<CR>` jump, and
>   a Tier-2 paint asserting an underlined error cell with the red `sp` color.

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

### Phase 3 — Go-to definition & references — ✅ DONE

> **Implementation notes (as built).** Faithful to the plan; choices worth
> recording:
> - **Request/reply plumbing lives in `nxvim-lsp`** as the design sketched:
>   `LspRequest` (definition/declaration/typeDefinition/implementation/
>   references, already in LSP coordinates), `ReqToken { kind: u16, generation:
>   u64 }` (the manager never interprets it — it only echoes it back), and
>   `LspReply::Locations(Vec<Location>)`. Every goto-family response shape
>   (`Location`, `Location[]`, `LocationLink[]`) and `references` is **normalized
>   to a flat `Vec<Location>` inside the manager** (`LocationLink` collapses to
>   its `target_selection_range`), so the editor handles one shape. Each request
>   is awaited on a **cloned `ServerSocket` in a detached task**, so a slow
>   round-trip never stalls the per-server serve loop and the editor never blocks
>   (Decision 3); the resolved value is forwarded as `LspEvent::Reply`.
> - **Stale-drop is the editor's job, by token *and* cursor.** The server keeps
>   one in-flight request per `LspReqKind` with the `(generation, buffer,
>   cursor)` it was issued at. A reply is dropped when its generation is behind
>   the latest of its kind (a newer request superseded it) **or** the buffer/
>   cursor changed since it was issued — so "fire `gd`, move, reply arrives" never
>   jumps. `$/cancelRequest` is left to a follow-up; the token drop already makes
>   stale replies harmless.
> - **Keymaps are intercepted in the server, not core** (core stays LSP-free, per
>   the design's "no other core changes"). `Server::input` runs a tiny prefix
>   recognizer: `gd`/`gD`/`gr` fire definition/declaration/references. `g` is a
>   two-key prefix, so it is withheld and the next key decides — a non-LSP second
>   key **replays the withheld `g`** before feeding the current key, so `gg`/`ge`/
>   `dgg`/… are untouched (operator-pending reports `Normal`, but it takes the
>   replay path). `g` is only armed in plain normal mode, leaving insert-mode text
>   and visual `g`-commands alone. Revisited once `vim.keymap` lands (Phase 7).
> - **Cross-file jumps refine the column after opening.** `jump_to` takes a byte
>   column, but converting the target's LSP character needs the target line —
>   which may be in a file the jump just opened. So a go-to does `jump_to(path,
>   line, 0)` to open/land on the line, reads the now-loaded line, converts
>   char→byte through the negotiated encoding, then `jump_to`s again (already
>   current ⇒ just moves the cursor, so the alternate `#` is recorded once). A
>   test proves a utf-16 cross-file definition lands on the right **byte** column
>   past a 2-byte `é`.
> - **Ex-commands too:** `:LspDefinition` / `:LspDeclaration` /
>   `:LspTypeDefinition` / `:LspImplementation` / `:LspReferences` route to the
>   same `request_lsp(kind)` path (the keymap-free entry, and the home for the two
>   goto-family members without a default key). A single definition jumps; multiple
>   results — and all references — open a **navigable** panel as a `path:line:col`
>   location list (`Editor::set_panel_targets` attaches the per-row jump targets;
>   a `<CR>` `jump_to`s in the core). An empty reply shows a brief "No definition
>   found"/… message.
> - **Navigable panels supersede `lsp_panel_locations`.** The Phase-2 server-side
>   location list (keyed by panel-select index, cleared on jump) was replaced by
>   making the panel itself carry per-line jump targets in the core. This both
>   removes the cross-layer select bookkeeping *and* fixes a reopen bug: because
>   the targets live in the `Panel`, they travel with the `:panelopen` snapshot,
>   so a references list dismissed by a `<CR>` jump still navigates when reopened.
> - **Mock** gained `definition`/`declaration`/`type_definition`/`implementation`/
>   `references` script fields (returned verbatim for the matching request).
>   Tests (in `crates/nxvim/tests/lsp.rs`) cover the same-file `gd` jump (asserting
>   the request was actually sent), a utf-16 cross-file `gd`, the `gr` references
>   panel + `<CR>` jump, the empty-reply message, and the cursor-moved stale drop
>   (`gdj` issues at (0,0) then moves before the reply, which must not jump).

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

### Phase 4 — Hover & signature help — ✅ DONE

> **Implementation notes (as built).** Faithful to the plan; choices worth
> recording:
> - **Request/reply reuses the Phase-3 token plumbing verbatim.** Two new
>   `LspRequest` variants (`Hover`, `SignatureHelp`) ride the same
>   `ReqToken`/generation path and the same per-`LspReqKind` stale-drop (by
>   generation *and* cursor) — so a hover/signature reply that arrives after the
>   cursor moved is discarded, exactly like go-to. No new staleness machinery.
> - **The reply is distilled in the manager, not the editor.** `LspReply` gained
>   `Hover(Vec<String>)` (the markup extracted to plain display lines — a
>   `MarkedString`, an array joined by blank lines, or a `MarkupContent.value`,
>   with trailing blanks trimmed) and `SignatureHelp { signature, active_parameter
>   }` (the active signature's label + its active parameter's text). Every protocol
>   response shape collapses inside `nxvim-lsp` before it reaches the editor, the
>   way goto responses already normalize to a flat `Vec<Location>`. The editor
>   does **no** markup parsing.
> - **Hover → the panel; signature help → the message line.** Hover docs can be
>   multi-line, so they open the bottom panel (`"LSP hover"`, non-navigable). A
>   signature is one line and is wanted *while typing*, so it renders on the
>   message line via `echo` — `format!("{signature}    [{param}]")`, the active
>   parameter bracketed since a plain message line can't style it inline. An empty
>   reply (`Hover(vec![])` / both fields `None`) shows a brief "No hover
>   information" / "No signature help" instead of an empty panel.
> - **Active parameter:** the per-signature `activeParameter` (LSP 3.16+) wins
>   over the top-level one; `ParameterLabel::Simple` is used verbatim,
>   `LabelOffsets` are sliced out of the signature label on char boundaries
>   (UTF-16 units, exact for ASCII, best-effort otherwise — display only).
> - **Keymaps, in the server (core stays LSP-free).** `K` (normal mode) fires
>   hover; `<C-k>` (insert mode) fires signature help. `K` joins the `g`-prefix
>   recognizer's normal-mode arm (it was previously an unbound no-op); `<C-k>` is
>   intercepted in insert mode *before* the editor, where it would otherwise
>   insert a literal `k`. Both have ex-command twins (`:LspHover`,
>   `:LspSignatureHelp`) — the keymap-free path. Signature help is **manual-only**;
>   auto-trigger on `(`/`,` is deferred to keep insert mode untouched until
>   completion (Phase 5), as the plan directs.
> - **Mock** gained `hover` and `signature_help` script fields (returned verbatim
>   for the matching request). Tests (in `crates/nxvim/tests/lsp.rs`) cover the `K`
>   hover panel (markdown → plain lines, trailing blank trimmed, request actually
>   sent), the empty-hover message (no panel), and the `<C-k>` signature line with
>   the active parameter bracketed (asserting no literal `k` was inserted).

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

### Phase 5 — Completion (the popup menu) — ✅ DONE

> **Implementation notes (as built).** Faithful to the refined plan; choices
> worth recording:
> - **The `pmenu` redraw key + a bordered ratatui overlay.** A new top-level
>   `pmenu` redraw key — `Nil` when closed, else `{items, selected, row, col,
>   width, height}` — projected each frame the way `diagnostics`/`panel` are.
>   `items` are `[label, kind, detail]` tuples (the ranked, filtered visible set);
>   `selected` is `Nil` until the user navigates; `row`/`col` anchor the box one
>   row below the cursor at the **word-start screen column** (`col` reuses
>   `cursor_screen_col`'s `virtcol` math, so no core change — the client adds the
>   number gutter). The TUI draws a `Borders::ALL` box (`Clear`ed first so it's
>   opaque), the selected row reverse-highlighted, **last** so it floats over the
>   text. The box flips above the cursor when there's no room below, and the list
>   scrolls to keep the selection visible. The `kind`/`detail` ride the protocol
>   but the minimal widget paints the label only (a kind icon is a follow-up).
> - **Reuses the Phase-3 token plumbing.** `LspRequest::Completion` /
>   `LspReply::Completion { is_incomplete, items }` ride the same
>   `ReqToken`/generation path and per-`LspReqKind` pending slot as goto/hover —
>   so re-requests supersede by generation and a stale reply is dropped. The
>   manager distills `CompletionItem[]`/`CompletionList` to a flat
>   `Vec<CompletionItemData>` (label, kind, detail, sort/filter/insert text, and
>   the `textEdit`/`additionalTextEdits` normalized to plain `TextEdit`s with
>   ranges **still in the negotiated encoding** for the editor to convert).
> - **Completion is the one reply that follows the moving cursor.** Unlike
>   goto/hover (dropped on a cursor move), a completion reply is dropped only on a
>   **buffer change** — the menu tracks the cursor as you type, so each keystroke
>   may re-request without the in-flight reply being discarded. It is also dropped
>   if the user has left insert mode by the time it lands (the menu is unwanted).
> - **Ordering is a deterministic tier sort.** Per item, a match tier — `0` exact,
>   `1` case-sensitive prefix, `2` case-insensitive prefix, `3` case-insensitive
>   subsequence, else **dropped** — then `(tier, sortText‖label, label)` ascending.
>   An empty prefix keeps everything at tier ≤ 1, so a just-triggered menu shows
>   the whole list in `sortText` order. (Advanced fuzzy scoring stays out.)
> - **Stay-open refresh.** A word char / `<BS>` while open edits the buffer first,
>   then re-ranks **in place**: a *complete* list refilters the cache client-side
>   (no request); an *incomplete* one fires a fresh request and keeps the current
>   items showing until the reply re-ranks them — so the `pmenu` key never goes
>   `Nil` mid-refresh. The menu closes on a non-word char, the cursor leaving the
>   word, leaving insert, or a complete list filtering to empty.
> - **Accept = a shared edit applier in core.** A new **LSP-free**
>   `Editor::apply_edits(edits, cursor_byte)` applies non-overlapping byte-range
>   replacements highest-start-first (so earlier offsets stay valid), `normalize`s,
>   and sets the cursor — **one undo step** that folds into the surrounding insert
>   block (so a single `u` reverts the accept *and* the typed prefix, as in vim).
>   The server converts the item's edit ranges (encoding-aware), computes the final
>   cursor as "end of the primary insertion, shifted by edits before it", and calls
>   it. Phase 6 generalizes the same applier to `WorkspaceEdit`.
> - **Keys, in the server (core stays LSP-free).** The insert-mode arm of
>   `lsp_keymap` owns the popup: `<C-x><C-o>` (a `<C-x>`-armed two-key prefix) and
>   `<C-Space>` trigger; while open, `<C-n>`/`<C-p>`/`<Down>`/`<Up>` navigate,
>   `<CR>`/`<Tab>`/`<C-y>` accept, `<Esc>`/`<C-e>` dismiss (`<Esc>` also leaves
>   insert), a word char / `<BS>` refreshes, and any other key dismisses then takes
>   its normal effect (so `<C-k>` signature help still fires after closing).
> - **Mock** gained `completion` (one scripted response for every request) and
>   `completion_sequence` (one response **per request**, for the re-request path).
>   Tests (in `crates/nxvim/tests/lsp.rs`) cover the headline ordering (`use nv` →
>   `nva`,`nvb`, one request), `sortText`-over-label ranking with a subsequence
>   tail, the `isIncomplete` live re-request (menu stays open, second request,
>   narrowed items), accept replacing the prefix + applying an `additionalTextEdit`
>   with single-undo, navigate/dismiss leaking no literal char, a Tier-2 bordered
>   paint, a utf-16 `textEdit` landing at the right byte past `é`, and a
>   never-blocks resilience check.

> **Plan refined 2026-06-03.** Two behaviors the original phase under-specified
> are now first-class, each with tests that pin it: the menu is **ordered by
> importance** (the typed prefix filters and ranks, so `use nv` surfaces
> `nva`/`nvb` and never `self`/`pub`), and once shown the menu **stays open and
> refreshes live** as you keep typing rather than flickering closed. Deterministic
> prefix + subsequence ranking with a `sortText` tiebreak moves from *Scope out*
> into *Scope in*; only advanced fuzzy/typo scoring and snippet expansion stay
> deferred. *(The stale `feature/lsp-completion` branch is **not** a reference — it
> predates this refinement and is ignored.)*

**Goal / value.** The big UI lift and the headline feature:
`textDocument/completion` driven from insert mode, shown in a **new popup-menu
(pmenu) surface** that filters, ranks, and live-refreshes as you type; accepting
an item replaces the typed word with its text (+ `additionalTextEdits`).

**Prerequisites.** Phases 1–3 (sync, tokens, byte↔encoding position conversion,
the `jump_to`/edit-application groundwork).

**The two behaviors to nail (this refinement's focus).** They are the acceptance
bar — what makes a completion menu feel real, and the easiest things to get subtly
wrong:
1. **Ordered by importance.** With the cursor after `use nv`, the menu shows the
   items whose name matches `nv` — `nva`, `nvb` — *ahead of and to the exclusion
   of* irrelevant names like `self` or `pub`. "Importance" is, in order: how well
   the item matches what was typed (exact/prefix beats a mere subsequence beats no
   match — which is dropped), then the server's own `sortText` priority, then the
   label alphabetically as a stable tiebreak. The menu is **never** left in raw
   server-array order once a prefix is typed.
2. **Stays open and refreshes live.** Triggering opens the menu once; **typing
   more word characters keeps the same menu open and updates its contents in
   place** — it does not close and reopen. Each keystroke recomputes the prefix
   and re-ranks: if the last list was complete (`isIncomplete: false`) the
   refilter is **client-side** against the cached items (no new request); if it was
   incomplete (`isIncomplete: true`) a **fresh `textDocument/completion`** fires at
   the new cursor and its result replaces the list when it lands. Typing a non-word
   character, moving the cursor out of the word, leaving insert mode, or an empty
   result set dismisses it.

**Scope (in):**
- **The pmenu surface (redraw key + widget).** A new top-level `pmenu` redraw key
  — `Nil` when closed, else a map the server projects each frame, the way it
  projects `diagnostics`/`panel`:
  - `items`: the ranked, filtered visible items, each `{label, kind, detail?}`
    (`kind` = `CompletionItemKind` as a small int → the client maps it to an
    icon/letter; `detail` is the right-aligned type/source hint when present);
  - `selected`: the selected index, `Nil` until the user navigates (so accept can
    fall back to the first item);
  - `row`, `col`: the anchor — the screen cell of the **start of the completion
    word** (one row below the cursor; flipped above if there's no room), so the
    menu's left edge lines up under the word being completed. `col` =
    `cursor_screen_col` minus the prefix's screen width; `cursor_screen_col`
    already rides the redraw, so **no core change** for the anchor;
  - `width`, `height`: dimensions clamped to the viewport (the list scrolls when
    taller than `height`).
  A minimal **ratatui pmenu widget** in the TUI floats this over the text area at
  `(row, col)` — the first overlay widget: a bordered list, the `selected` row
  reverse-highlighted, drawn **last** so it sits above the text. Core stays out of
  it: the server owns the menu model and drives it from insert-mode state, exactly
  as it owns diagnostics.
- **Request/reply plumbing.** A new `LspRequest::Completion { uri, position }` and
  `LspReply::Completion { is_incomplete, items }` distilled in the manager
  (normalize the `CompletionItem[]` vs `CompletionList` response shapes; reduce
  each item to `{label, kind, detail, filter_text, sort_text, insert_text,
  text_edit, additional_text_edits}` with ranges still in the negotiated encoding
  for the editor to convert). Rides the existing `ReqToken`/generation path and
  per-kind stale-drop — a reply for a superseded generation, or one that arrives
  after the menu closed, is dropped.
- **The completion word (prefix).** A pure server helper: the run of identifier
  characters (`[A-Za-z0-9_]`) immediately left of the cursor on the current line,
  in bytes. It is both the **filter string** and the **default replace range**
  (word-start..cursor) for items carrying only `insertText`/`label`; an item with
  an explicit `textEdit` uses the server-provided range instead.
- **Filtering & ranking (the ordering).** Client-side, deterministic, table-free,
  re-run whenever the item set or prefix changes. For typed prefix `p` and an
  item's filter string `f` (= `filterText` else `label`):
  - compute a **match tier** — `0` exact (`f == p`), `1` case-sensitive prefix,
    `2` case-insensitive prefix, `3` `p` a case-insensitive subsequence of `f`,
    else **drop the item**;
  - sort survivors by the tuple `(tier, sort_text_or_label, label)` ascending — so
    match quality dominates, the server's `sortText` orders items of equal quality,
    and the label is the final stable tiebreak.
  An empty prefix (just triggered, nothing typed) keeps every item (all tier `≤2`
  against the empty string) in `sortText` order — the correct "show everything the
  server offered, in its priority" initial state. *(This is the "ordered by
  importance" contract; advanced fuzzy scoring — typo tolerance, gap/cluster
  penalties — is out, see below.)*
- **The menu state machine (server-owned, in `lsp.rs`/`lib.rs`).** A
  `CompletionMenu` holds the raw last list, `is_incomplete`, the anchor (buffer row
  + word-start byte col), the live prefix, the ranked visible items, and the
  selected index. Insert-mode keys are intercepted while it is open, extending the
  `lsp_keymap` insert-mode arm that already owns `<C-k>`:
  - **trigger** `<C-x><C-o>` / `<C-Space>`: fire a request at the cursor; on reply,
    build the menu and project `pmenu`. (Auto-trigger on a typed identifier char is
    a follow-up *toggle* once manual is solid — but **live refresh while already
    open is in scope here**, since that is the headline behavior.)
  - **word char** while open: let the editor insert it, then recompute the prefix
    and **refresh in place** — refilter the cache when the last list was complete,
    else re-request. The menu closes for a word char only if the result is empty.
  - **`<BS>`** while open: editor deletes, prefix shrinks, refresh (re-request if
    the prior list was incomplete); closes if the cursor backs out of the word.
  - **`<C-n>`/`<C-p>`/`<Down>`/`<Up>`**: move `selected` (wrapping); no buffer
    change, menu stays open.
  - **`<CR>`/`<Tab>`**: accept the selected (or first) item, then close.
  - **`<Esc>`/`<C-e>`**: close without inserting (`<Esc>` then also leaves insert,
    as usual).
  - any other key, a cursor move, or a mode change: close, then let the key take
    its normal effect.
- **Accept = an edit applier.** Convert each edit's LSP range → byte range through
  the negotiated encoding, apply via `Buffer::insert`/`remove` (in reverse order
  within a document so earlier offsets stay valid), `normalize()`, and set the
  cursor to the end of the primary insertion. One accept is **one undo step** and
  flows to the next `didChange` through the shared edit journal (`take_edits`), so
  the server's document version stays consistent. This is the same applier
  Phase 6 generalizes to multi-file `WorkspaceEdit`s. (Setting the cursor and
  grouping the undo are editor-domain, not LSP — any small core helper added here
  takes no LSP types, keeping `nxvim-core` LSP-free.)

**Scope (out):**
- **Snippets** (`InsertTextFormat.Snippet` placeholder/tab-stop expansion) —
  insert the snippet's plain text for now; expansion is a cross-phase follow-up.
- **Advanced fuzzy ranking** — typo tolerance and gap-weighted scoring beyond the
  prefix + subsequence + `sortText` order above.
- **`completionItem/resolve`** for lazy `documentation`/`detail` — optional; wire
  it only if trivial, else defer (the menu shows the eager `detail`).
- **Float chrome** — the pmenu is a minimal overlay until real float layout exists
  (cross-phase note); no documentation popup beside the menu yet.

**Files.** `crates/nxvim-lsp/src/manager.rs` (the `Completion` request + the
`CompletionList`/item distillation) and `crates/nxvim-lsp/src/mock.rs` (the
`completion` + `completion_sequence` script fields, below); `crates/nxvim-server/src/lsp.rs`
(prefix, ranking, menu model, accept applier); `crates/nxvim-server/src/lib.rs`
(the `pmenu` redraw key + the insert-mode menu state machine + key interception);
`crates/nxvim-tui/src/{view.rs, render.rs}` (the `pmenu` `View` field + the overlay
widget); `crates/nxvim/tests/lsp.rs`, plus a Tier-2 screen test in
`crates/nxvim/tests/`. **No core change** is anticipated (the anchor reuses
`cursor_screen_col`; ranking/menu live in the server) beyond, at most, a tiny
LSP-free cursor-set / undo-group helper for accept.

**Mock additions.** To drive ordering and live-refresh deterministically, the mock
gains two script fields (mirroring the existing `definition`/`hover` fields):
- `completion`: a single scripted response — a `CompletionItem[]` or a
  `CompletionList` (`{isIncomplete, items}`) — returned for every
  `textDocument/completion` (the client-side-filter, ranking, accept, and encoding
  tests use this).
- `completion_sequence`: an array of responses consumed **one per
  `textDocument/completion` request**, overriding `completion` when present — so a
  test returns a broad `isIncomplete: true` list first and a narrowed list on the
  re-request, proving the live re-request path. (The mock already records every
  request, so a test can also assert *how many* completion requests were sent.)

**Tests.** A new `pmenu_of(params) -> Option<(Vec<String> /*labels*/, i64
/*selected*/)>` helper mirrors `panel_of`/`diagnostics_of`, and a `wait_for_pmenu`
poller mirrors `wait_for_panel`. Then:
- **Ordering by importance — the headline (`use nv` → `nva`, `nvb`, not `self`/
  `pub`).** Buffer `use ` (cursor at end, insert mode); `completion` returns, in a
  deliberately unhelpful order, `[pub, self, nvb, nva]` with `isIncomplete:false`.
  Trigger, then `feed("nv")`. Assert the `pmenu` `items` are **exactly**
  `["nva", "nvb"]` in that order with `self`/`pub` absent; the menu is **still
  open** across both keystrokes; and **only one** `textDocument/completion` was
  recorded (a complete list ⇒ client-side filter, no re-request).
- **Ranking honors `sortText` over the label.** Two prefix-matching items whose
  `sortText` order is the reverse of their alphabetical order (label `config`
  `sortText:"2"`, label `connect` `sortText:"1"`); type their shared prefix and
  assert the menu lists `["connect", "config"]` — `sortText` wins, so the order is
  by importance, not alphabet. Include a third, subsequence-only item and assert it
  ranks **below** both prefix matches.
- **Stays open and refreshes live from the server (`isIncomplete`).**
  `completion_sequence` = `[broad isIncomplete:true list, narrowed isIncomplete:true
  list]`. Type `n`, trigger → menu open with the broad items. `feed("v")` → assert
  the menu **stayed open** (the `pmenu` key never went `Nil` between redraws), a
  **second** `textDocument/completion` was sent, and the items are now the narrowed
  set — a live server refresh, not just a client filter.
- **Accept inserts the item + `additionalTextEdits`.** `completion` item
  `{label:"println", insertText:"println", additionalTextEdits:[insert
  "use std::io;\n" at line 0]}`; trigger, `<C-n>` to select, `<CR>`. Assert the
  typed word became `println` (prefix **replaced**, not appended) **and** the
  import line was added; the menu closed; a single undo restores both.
- **Navigate, and dismiss without inserting.** After a trigger, `<C-n>` moves
  `selected` (assert the index); `<C-e>` closes the menu with the buffer unchanged;
  separately `<Esc>` closes it and leaves insert mode, buffer unchanged — no
  control key leaked a literal character (the `<C-k>` test's "no literal `k`"
  pattern).
- **Tier-2 screen paint.** The pmenu paints as a bordered overlay anchored at the
  word start (`col` = gutter + word-start screen col, one row below the cursor),
  the selected row reverse-highlighted, the cells under it belonging to the menu
  (not the text). Assert specific cells and the selected-row style, the
  `a_diagnostic_cell_is_painted_with_an_underline` pattern.
- **Encoding.** On a line with a leading 2-byte `é` and a `utf-16` server, accept an
  item whose `textEdit` range is in utf-16 units; assert the replacement lands at
  the right **byte** offset (the completion analogue of the cross-file `é` test).
- **Resilience.** A completion request whose reply never arrives (or arrives after
  the menu was dismissed) leaves the editor fully editable and inserts nothing; a
  stale reply (generation superseded by the re-request) is dropped.

**Done when.** All of the above pass; `nxvim-core` still carries no LSP/async/JSON
deps; the three workspace gates are green. *(Largest phase: if it overflows a
context, split at the pmenu-widget boundary — 5a = surface + widget + manual
navigation against static mock items, **including the ordering/ranking and the
stay-open client-side refilter**; 5b = the `isIncomplete` re-request live refresh +
accept/edit application + encoding. The mock fields and the `pmenu` redraw shape
land in 5a so 5b is purely behavior.)*

---

### Phase 6 — Edits: formatting, rename, code actions — ✅ DONE

> **Implementation notes (as built).** Faithful to the refined plan; choices
> worth recording:
> - **The multi-buffer applier is a new, LSP-free core entry.**
>   `Editor::apply_edits_to(id, edits)` applies a document's byte-range edits to a
>   **given** `BufferId` (current or not) as **one independent undo step for that
>   buffer** — a sibling of Phase 5's current-buffer `apply_edits`, built on a new
>   `push_undo_for(id)` that snapshots an arbitrary buffer's history (it does *not*
>   consult the current buffer's `snapshot_taken` insert-group flag, since a
>   workspace edit is a one-shot normal-mode mutation per buffer). Edits apply
>   highest-start-first; the current buffer's cursor is clamped to the new text, a
>   non-current buffer's saved cursor is clamped on the switch back by
>   `enter_buffer`. Three small read-only core accessors support the server side:
>   `buffer_of(id)` (read a non-current buffer's text to convert LSP positions),
>   `buffer_id_for_path(path)` (route a WorkspaceEdit URI to its open buffer), and
>   `take_lsp_edits_of(id)` (drain a non-current buffer's LSP journal) — plus
>   `panel_title()` (so the server recognizes the code-action panel). Core gains
>   **no** LSP types.
> - **`WorkspaceEdit` normalization lives in `nxvim-lsp`** (the goto/hover
>   pattern): `changes` and the versioned `documentChanges` (collapsing
>   `OneOf<TextEdit, AnnotatedTextEdit>`, dropping file create/rename/delete
>   resource ops) both reduce to a flat `WorkspaceEditData = Vec<(Url,
>   Vec<TextEdit>)>`, ranges left in the negotiated encoding. `CodeActionData
>   { title, edit: Option<WorkspaceEditData> }` keeps only the eager edit (a bare
>   `Command` or a lazy `edit: None` action carries `None` — `codeAction/resolve`
>   and `workspace/executeCommand` stay out, per Scope).
> - **A workspace edit is matched to its open buffer by URI, not by path.** Each
>   URI is resolved to a buffer the way diagnostics are: an exact match against the
>   `file://` URI sent at `didOpen` (always absolute, so a buffer opened by a
>   *relative* path still matches — the early bug here was a lexical path compare),
>   with a canonicalized-path fallback for a server that resolves symlinks in its
>   returned URI (e.g. macOS `/var` → `/private/var`). Only open buffers are
>   edited; an unopened-file URI is skipped (Scope out).
> - **A per-buffer `sync_lsp_buffer(id)`** flushes `didChange` for each
>   non-current buffer a workspace edit touched (the current buffer is delegated to
>   the normal `sync_lsp`, so each journal entry reaches exactly one `didChange`).
>   It drains that buffer's LSP journal and sends incremental deltas (or full text
>   on `resync`/`FULL`) against *that* buffer's text + encoding, bumping its
>   version — built on buffer-addressed free functions (`lsp_range_to_bytes_in` /
>   `lsp_position_in` / `incremental_changes_in` taking a `&Buffer`), to which the
>   current-buffer methods now delegate.
> - **Stale-drop for an apply is content-version based.** `PendingLspReq` gained a
>   `tick` (the buffer's `changedtick` at issue time); an `Edits`/`WorkspaceEdit`/
>   `CodeActions` reply is dropped when the buffer changed since the request
>   (`buffer_changed || tick_changed`) — a cursor move alone is fine to apply over,
>   unlike the goto/hover kinds. Proven by a test that delays the formatting reply
>   (a new mock `reply_delay_ms`), edits in the gap, and asserts the edit stands.
> - **Capabilities advertised at `initialize`:** `textDocument.formatting`,
>   `textDocument.rename`, `textDocument.codeAction.codeActionLiteralSupport` (the
>   standard kind set — without it a server returns legacy `Command[]` with no
>   edit), and `workspace.workspaceEdit { documentChanges: true }`.
> - **Three ex-commands, each with its own issue function** (the uniform
>   `request_lsp(kind)` `{uri, position}` path doesn't fit): `:LspFormat` (sends
>   `FormattingOptions { tab_size: 8, insert_spaces: true }`, fixed until `:set`
>   lands), `:LspRename {newname}` (reads the dispatcher's arg; empty ⇒ `E471`),
>   and `:LspCodeAction` (a point range at the cursor + the diagnostics there as
>   context — a visual-selection range is a follow-up). A shared
>   `register_lsp_request(kind)` centralizes the generation/buffer/cursor/tick
>   bookkeeping all issue functions share.
> - **Code actions use the `panel_selects` path** (per the design's note): the
>   titles open a select-enabled panel, the resolved actions are stashed in a
>   server-side `Vec<CodeActionData>` keyed by select index, and a `<CR>` is routed
>   to `apply_code_action` **only when the open panel's title matches** the
>   code-action panel (so a select on some other panel can't misroute). The action
>   applies via the same `apply_workspace_edit` rename uses; the panel closes.
> - **`format_on_save` was descoped** to a cross-phase follow-up (it inverts the
>   core-owned, synchronous `:w` and needs a deferred-write pre-write hook).
> - **Mock** gained `formatting` / `rename` / `code_action` script fields (via the
>   existing `reply_scripted` path) plus `reply_delay_ms`. Tests (in
>   `crates/nxvim/tests/lsp.rs`) cover `:LspFormat` rewrite + idempotent re-run, the
>   content-version drop, a **two-file** rename across open buffers (each
>   independently undoable, cursor survives, sibling read by handle), the
>   code-action panel + `<CR>` apply, a utf-16 formatting edit landing at the right
>   **byte** past `é`, and a format/rename never-blocks resilience check.

> **Plan sanity-checked & refined 2026-06-04** (against the Phase 1–5 code as
> built). Phase 6 is the **first feature that mutates buffers other than the
> current one**, and all the machinery built in Phases 1–5 is
> single-(current)-buffer-centric — so the original "the Phase 5 applier just
> generalizes" understated the work. Five things become first-class here, each
> grounded in the code:
> - **A multi-buffer applier is genuinely new (core) work.** Phase 5's
>   `Editor::apply_edits(edits, cursor_byte)` operates on the **current** buffer
>   only, as a single `push_undo`. A multi-file `WorkspaceEdit` must edit *other*
>   open buffers too, which needs a new LSP-free core entry that applies one
>   document's byte-range edits to a given `BufferId` (each buffer its own undo
>   step) — not the active-buffer helper. The rename test below ("each affected
>   open buffer changed") is exactly the case the Phase-5 applier cannot reach.
> - **Non-current buffers must be synced after the apply.** `sync_lsp` flushes
>   `didChange` for the **current** buffer only (it reads `self.editor.buffer()`).
>   After a workspace edit touches buffers B and C, their version/edits aren't
>   sent until each next becomes current — the server's view drifts. Phase 6 needs
>   a per-buffer `sync_lsp_buffer(id)` it calls for every touched document, which
>   is what makes the applier's "bump versions so the next `didChange` is
>   consistent" actually true.
> - **The stale-drop must be content-version-based for an *apply*.** Goto/hover
>   drop a reply when the cursor moved; formatting/rename return whole-document
>   edits computed against the **request-time text**, so applying them after any
>   edit corrupts the buffer. Capture the buffer's `changedtick` with the pending
>   request and drop the reply if it advanced (a cursor move alone is fine to
>   apply over). `PendingLspReq` carries `buffer`+`cursor` today; add the tick.
> - **`format_on_save` is not a flag — it inverts `:w`.** `:w` is owned by
>   **core** (`execute_ex` → `ex_write`), writes synchronously, and the server
>   only learns of the save *after the fact* via `save_tick`; there is **no**
>   pre-write hook, and formatting is async (reply-as-event, never block). Correct
>   format-on-save would intercept `:w`, request formatting, and write to disk
>   **only when the reply lands** — a deferred write across a round-trip plus a
>   new core pre-write hook. That is its own design, so it is **descoped from
>   Phase 6 to a follow-up**; Phase 6 ships the explicit, request-driven
>   `:LspFormat` only.
> - **Capabilities must be advertised or the features arrive unusable.** Servers
>   gate these on the client capabilities in `initialize` (Phase 1 advertised only
>   sync + `positionEncodings`). Most important:
>   `textDocument.codeAction.codeActionLiteralSupport` — **without it a server
>   returns legacy `Command[]`, not a `CodeAction` carrying an `edit`**, and
>   "apply the edit" is impossible. Also `workspace.workspaceEdit`
>   (`documentChanges`), `textDocument.formatting`, `textDocument.rename`.

**Goal / value.** Buffer-mutating features: `textDocument/formatting`,
`textDocument/rename` (apply a `WorkspaceEdit` across the **open** buffers it
touches), `textDocument/codeAction` (list in the panel, apply the chosen action's
edit). Range/on-type formatting and format-on-save are follow-ups.

**Prerequisites.** Phases 1–5 — document sync, the `ReqToken`/`Reply` infra and
per-kind stale-drop, byte↔encoding position conversion, and the Phase-5
per-document `apply_edits` this phase lifts into a multi-buffer applier.

**Scope (in):**
- **Advertise the gating client capabilities** (extend the Phase-1 `initialize`):
  `textDocument.formatting`, `textDocument.rename`,
  `textDocument.codeAction.codeActionLiteralSupport` (else `Command[]`, see
  above), and `workspace.workspaceEdit { documentChanges: true }` (resource ops
  stay out — Scope out).
- **A shared `WorkspaceEdit` applier (the keystone).** In `nxvim-lsp`, normalize a
  `WorkspaceEdit` — 0.95.1 carries it as **either** `changes: {Url → TextEdit[]}`
  **or** `document_changes` (`Edits(TextDocumentEdit[])`, whose edits are
  `OneOf<TextEdit, AnnotatedTextEdit>`, or `Operations` that also mix in file
  create/rename/delete) — into a flat `Url → Vec<TextEdit>` (collapse the
  `OneOf`/annotation; **drop resource ops**, the scoped-out unopened-file case),
  ranges left in the negotiated encoding (the goto/hover normalization pattern).
  In `nxvim-server`, for each URI that maps to an **open** buffer: convert ranges →
  bytes through *that* document's server encoding + line text, apply via the new
  per-`BufferId` core entry (reverse order within a document so earlier offsets
  stay valid; one undo step per buffer), then `sync_lsp_buffer(id)` so its
  `didChange`/version stay consistent. (For the current buffer this is exactly
  Phase 5's accept path, so it gets the journal/syntax resync for free; the new
  work is reaching the *other* buffers.)
- **`:LspFormat`** → `textDocument/formatting` (send `FormattingOptions` with a
  fixed default — `tabSize: 8` to match the `TABSTOP` constant, `insertSpaces:
  true` — since nxvim has no `:set shiftwidth`/`expandtab` yet; real options are a
  follow-up when `:set` lands) → on reply, apply the `TextEdit[]` to the current
  buffer **iff it hasn't changed since the request** (the version guard above);
  re-running on already-formatted text is a no-op.
- **`:LspRename {newname}`** → `textDocument/rename` (the new name rides the
  request; `:LspRename` reads the ex-command argument the dispatcher already
  splits off, as `:colorscheme` does) → apply the returned `WorkspaceEdit` across
  the open buffers.
- **`:LspCodeAction`** → `textDocument/codeAction` at the cursor (or the visual
  selection's range) with a `context.diagnostics` of the diagnostics there → list
  the result titles in the **panel**; on `<CR>`, apply the chosen action's `edit`.
- Because these three do **not** share the uniform `{uri, position}` shape of
  `request_lsp(kind)` (format is whole-document + options; rename adds a name; code
  action adds a range + diagnostics context), each gets its own small issue
  function alongside new `LspReqKind` / `LspRequest` / `LspReply` variants.

**Scope (out):**
- **Workspace edits to unopened files** and **resource operations**
  (`create`/`rename`/`delete` file in `documentChanges`) — apply only to
  already-open buffers; open-then-edit (or direct on-disk write) is a follow-up.
- **`codeAction/resolve`** — real servers often return a lazy `CodeAction` with
  `edit: None` + `data`, needing a resolve round-trip to populate the edit.
  Phase 6 applies only actions that arrive with an **eager** `edit` (the mock
  returns those); resolving lazy actions, and running an action's `command` via
  `workspace/executeCommand`, are follow-ups.
- **`format_on_save`** — inverts `:w` (see the refinement note); follow-up.
- **Range / on-type formatting.**

**Files.**
- `crates/nxvim-lsp/src/manager.rs` — the new `Formatting`/`Rename`/`CodeAction`
  requests and the `WorkspaceEdit` / `TextEdit[]` / `CodeAction[]` distillation —
  and `crates/nxvim-lsp/src/mock.rs` — the `formatting`/`rename`/`code_action`
  script fields (below). *(Both omitted from the original Files list; every prior
  feature phase touched them.)*
- `crates/nxvim-core/src/editor.rs` — the per-`BufferId` edit applier (LSP-free,
  the multi-buffer sibling of Phase 5's `apply_edits`).
- `crates/nxvim-server/src/lsp.rs` — the WorkspaceEdit→buffers apply driver, the
  per-buffer sync, the version-guarded reply handling, the three issue functions.
- `crates/nxvim-server/src/lib.rs` — the `:LspFormat`/`:LspRename`/`:LspCodeAction`
  ex-commands and the code-action panel-select→apply wiring.
- `crates/nxvim/tests/lsp.rs` — **not** `crates/nxvim-server/tests/` (the mock
  binary needs `CARGO_BIN_EXE_nxvim`, as Phases 1–5 record) — plus a Tier-2 screen
  test if the code-action panel warrants one.

> **Code-action panel payload.** The panel carries two payloads today: jump
> `targets` (which travel with the `:panelopen` snapshot) and the generic
> `panel_selects` select-event (server-side, index-keyed — the pattern Phases 2–3
> deliberately *replaced* for jumps because it drifts on reopen). "Apply this
> action's edit on `<CR>`" is neither a jump nor naturally snapshot-safe, so code
> actions use the `panel_selects` path with a server-side `Vec<resolved action>`
> keyed by the select index — and inherit its caveat: a code-action list dismissed
> and reopened via `:panelopen` may have lost its actions. Acceptable for the MVP
> (the list is ephemeral), noted so a future reader isn't surprised.

**Mock additions.** Mirroring the `definition`/`hover` fields, the mock gains
three script fields answered by the existing `reply_scripted` path:
- `formatting`: the `TextEdit[]` returned for `textDocument/formatting`.
- `rename`: the `WorkspaceEdit` returned for `textDocument/rename`.
- `code_action`: the `(CodeAction | Command)[]` for `textDocument/codeAction`
  (tests script `CodeAction`s carrying an eager `edit`).

**Tests** (in `crates/nxvim/tests/lsp.rs`).
- Mock returns formatting `TextEdit`s; `:LspFormat` rewrites the buffer to the
  expected lines; idempotent on re-run; **a reply that lands after an intervening
  edit is dropped** (version guard) and the buffer is left intact.
- Rename returns a **two-file** `WorkspaceEdit` for two **open** buffers; assert
  both buffers changed correctly, each is independently undoable, and the active
  buffer's cursor survives.
- Code-action list populates the panel; `<CR>` on a row applies that action's
  edit; no control key leaks a literal character.
- Encoding: a formatting/rename edit on a line with a leading 2-byte `é` (utf-16
  server) lands at the right **byte** offset (the cross-file-`é` analogue).
- Resilience: a format/rename request whose reply never arrives leaves the editor
  fully editable and the buffers unchanged.

**Done when.** The above pass; `nxvim-core` still carries no LSP/async/JSON deps;
the three gates are green. *(Sized like Phase 5 — if it overflows a context, split
at the feature boundary: **6a** = capability advertisement + WorkspaceEdit
normalization + the multi-buffer applier + per-buffer sync + version guard, proven
through `:LspFormat` and `:LspRename`; **6b** = `:LspCodeAction` (the
panel-select→apply payload, plus `codeAction/resolve` if pursued). The applier and
mock fields land in 6a so 6b is purely the code-action surface.)*

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
- **Format-on-save** (descoped from Phase 6) — must invert the `:w` flow: `:w` is
  core-owned and synchronous, formatting is async, so format-on-save intercepts
  the write, requests formatting, and writes to disk only when the reply lands (a
  deferred write + a new core pre-write hook). Its own design.
- **Lazy / command code actions** — `codeAction/resolve` for actions returned with
  `edit: None`, and running an action's `command` via `workspace/executeCommand`.
  Phase 6 applies only eager-`edit` actions.
- **Workspace edits to unopened files & resource operations** — `WorkspaceEdit`
  `create`/`rename`/`delete` file ops, and edits to files not currently open
  (open-then-edit or direct on-disk write). Phase 6 edits only open buffers.

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
