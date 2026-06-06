# Treesitter syntax highlighting — design

**Date:** 2026-06-01
**Status:** Implemented (crate `nxvim-ts`; tests in `crates/nxvim/tests/syntax.rs`)

## Goal

Add **syntax highlighting** to nxvim. nxvim is **treesitter-native only** — there
is no regex/`syntax.vim` highlighter and there never will be. Highlighting is
driven entirely by [tree-sitter](https://tree-sitter.github.io) grammars and
their `highlights.scm` queries, exactly as neovim's built-in treesitter does.

Two decisions shape the whole design (both chosen deliberately):

1. **Crash isolation via a separate process.** Tree-sitter grammars are compiled
   C. A buggy grammar can *segfault*, which no in-process `catch_unwind` can
   survive. So parsing runs in a **separate OS process** the editor spawns and
   supervises. If it dies — even by segfault — the editor keeps running, and the
   process is respawned. **Highlighting can never crash, stall, or even slow the
   editor.**
2. **Installable grammars, not bundled.** Like nvim-treesitter, grammars are
   **installed at runtime** into a data directory as compiled parser libraries
   plus `.scm` query files, and loaded dynamically by filetype. nxvim ships
   **zero** grammars linked into the binary. The on-disk layout is deliberately
   **nvim-treesitter-compatible**, so an existing nvim-treesitter `parser/` +
   `queries/` tree is drop-in usable.

**In scope (this feature):** the syntax process and its supervision/respawn; the
dynamic grammar loader; **incremental parsing** — a shadow buffer + persistent
parse tree in the worker, kept current by **edit deltas** (`InputEdit`), so huge
files stay responsive; the highlight RPC protocol; async, non-blocking
integration into the server loop; the `View`/`redraw` highlight payload;
filetype→language detection; the TUI's group→color theming and painting; a
default highlight-group palette.

**Out of scope (follow-ups, noted below):** `:TSInstall`/`:TSUpdate` (fetching &
compiling grammars from a registry); language injections beyond a single
grammar; highlighting the over-scan band *during* a scroll animation; a `:set`
toggle / per-buffer enable; configurable filetype and colorscheme.

> **Why incremental is non-negotiable here.** The target is *seeing it work on
> huge files*. Re-sending the whole buffer on every keystroke is O(file size) of
> transfer **and** O(file size) of re-read in the worker every key — the exact
> thing that makes big files lag. Tree-sitter is built for incremental reparse:
> given the old tree and an `InputEdit`, it reparses only the affected region.
> But to reparse it must still *read* the current text, so the worker keeps its
> own **shadow copy** of each buffer, synced by tiny deltas. Both the transfer
> and the reparse then scale with the *edit*, not the file. A 100k-line file
> parses once on open (async, off the keystroke path) and every edit after is
> microseconds.

---

## Architecture

```
┌────────────┐  redraw (+highlights)   ┌──────────────┐  ts_highlight (text)   ┌────────────────────┐
│ nxvim-tui  │ ◀────────────────────── │ nxvim-server │ ─────────────────────▶ │ syntax worker      │
│ (client)   │  ──── nvim_input ─────▶ │ (editor)     │ ◀── ts_highlights ──── │ (nxvim --__ts)     │
└────────────┘                         └──────────────┘   msgpack over pipe    │ tree-sitter +      │
   main thread        nxvim-rpc          its own thread       nxvim-rpc         │ libloading         │
                                              │ spawns/supervises               └────────────────────┘
                                              │                                    │ dlopen by filetype
                                              ▼                                    ▼
                                                                         <data>/parser/<lang>.{so,dylib,dll}
                                                                         <data>/queries/<lang>/highlights.scm
```

The syntax worker is **another nxvim-rpc peer**. We reuse the exact msgpack-RPC
framing the client↔server link already uses (`nxvim-rpc`), so there is no second
protocol stack — just a new set of methods. The **server is the client** of the
worker.

### One binary, worker mode

The worker is not a second executable. The `nxvim` binary, when invoked with an
internal flag (`nxvim --__ts-worker`, hidden from `--help`), runs the worker main
loop and never starts an editor. The server spawns it via
`std::env::current_exe()`, so there is nothing extra to install or find on
`$PATH`, and the worker is always version-matched to the server. The tree-sitter
and `libloading` dependencies are reached **only** in worker mode.

### Crate layout

One new crate, `nxvim-ts`, holds everything tree-sitter:

| crate            | new? | role                                                                                 |
| ---------------- | ---- | ------------------------------------------------------------------------------------ |
| `nxvim-ts`       | new  | The worker: `run_worker(stdin, stdout)`, the dynamic grammar loader, the parse/query engine, the data-dir resolver. Heavy C deps (`tree-sitter`, `tree-sitter-highlight`, `libloading`) live **here only**. |
| `nxvim-server`   | —    | Gains a `SyntaxClient`: spawns + supervises the worker subprocess, speaks the highlight protocol over its pipes (`nxvim-rpc` + `tokio::process`), caches spans, drives respawn. **Does not depend on `nxvim-ts`** — it only spawns a subprocess and speaks msgpack, so tree-sitter stays out of the server's build graph. |
| `nxvim` (bin)    | —    | Routes `--__ts-worker` to `nxvim_ts::run_worker`; otherwise starts normally. This is the *only* crate that depends on `nxvim-ts`. |
| `nxvim-core`     | —    | One additive change: a `changedtick` on `Buffer` (below). No tree-sitter, no highlight logic — stays pure & synchronous. |
| `nxvim-tui`      | —    | Maps highlight-group **names** → colors and paints them. Stays a thin client; learns nothing about tree-sitter. |

Dependency direction stays one-way. `nxvim-server` → (spawns) → `nxvim` worker
mode is a *process* edge, not a crate edge, so there is no cycle.

---

## Why a process, and why it never blocks the editor

The worker link is **fully asynchronous and advisory**. The editor never awaits
it; a redraw is emitted immediately with whatever spans are currently cached
(possibly empty or one tick stale). This is the same "server owns content, client
owns presentation; never block on the other side" rule the smooth-scrolling and
client-server designs already rest on — extended to a third party.

Flow on a buffer change:

1. An edit appends to `buffer`'s **edit journal** and advances `changedtick`. The
   server drains the journal and fires a **`ts_edit` notification**
   (fire-and-forget) carrying only the *deltas* (each: byte range removed, bytes
   inserted, and the tree-sitter positions), the new tick, and the visible line
   range. It does **not** wait. (On the *first* sync, or after an undo/file
   reload, it instead fires **`ts_open`** with the full text — see resync below.)
2. The server emits its redraw right away, carrying the **last** spans it has.
3. The worker applies the deltas to its **shadow buffer** and to the old tree
   (`tree.edit`), **reparses incrementally** (`parse_with(.., Some(&old_tree))`),
   runs the highlights query over just the requested line range, and sends a
   **`ts_highlights` notification** back, tagged with the tick it parsed.
4. The server stores those spans and emits a **fresh redraw** so the client
   repaints with correct colors.

Highlighting therefore "catches up" a frame or two behind fast typing — exactly
how real editors feel — while keystroke→buffer→redraw latency is untouched, and
**per-edit work scales with the edit, not the file size.**

**Scroll with no edit:** scrolling changes the visible range but not the text. The
server fires **`ts_view`** (just the buffer id + new line range); the worker
re-runs the query over the new range against the *existing* tree (no reparse) and
replies with `ts_highlights`. So newly-revealed lines colorize as they scroll in.

**Debounce / coalescing:** at most one request is in flight per buffer. Deltas
that accumulate while one is outstanding are batched and sent when the reply
returns; the tick they carry lets the worker drop a superseded request. Because
deltas are tiny, the journal never grows unbounded during fast typing.

### Supervision & respawn (the crash-proof part)

`SyntaxClient` owns the child and its pipe. It survives the worker dying in any
way:

- **Detection:** the worker's stdout closes / the read task ends, or a write
  fails (broken pipe). Either signals the child is gone.
- **Respawn with backoff:** the client restarts the worker. A **crash counter**
  (e.g. ≥ 3 deaths within 10 s) trips a circuit breaker that stops respawning for
  a cool-down — preventing a crash-loop from a poison grammar burning CPU. The
  breaker resets after a quiet period or a successful highlight.
- **Poison-grammar guard:** if loading or parsing a *specific* language keeps
  killing the worker, the client stops requesting **that language** (per-language
  strike count) while keeping the worker alive for others.
- **Defense in depth:** inside the worker, parse/query/`configure` calls are
  wrapped in `catch_unwind`, so a Rust-level *panic* degrades to "no spans for
  this request" without even dropping the process. Only a hard C segfault escalates
  to the respawn path.

Throughout, **editing is unaffected**: the editor thread never references the
worker except to hand it text and receive spans, both asynchronously.

---

## Grammar installation model (nvim-treesitter-compatible)

### Data directory

Resolved once at startup, overridable by `$NXVIM_DATA_DIR` (essential for tests):

- Linux/BSD: `$XDG_DATA_HOME/nxvim` else `$HOME/.local/share/nxvim`
- macOS: same XDG rule (kept simple; not `~/Library/Application Support`)
- Windows: `%LOCALAPPDATA%\nxvim`

No new dependency — a tiny env-based resolver. Layout (identical to neovim's
`runtimepath` parser/query convention):

```
<data>/
  parser/
    rust.so          # Linux: .so   macOS: .dylib   Windows: .dll
    python.so
  queries/
    rust/
      highlights.scm
      injections.scm # optional
    python/
      highlights.scm
```

Because this matches nvim-treesitter exactly, a user can **copy or symlink their
existing `~/.local/share/nvim/.../parser/*.so` and the queries** and get
highlighting immediately — that is the v1 install story until `:TSInstall` lands.
The compiled-parser ABI is the standard tree-sitter one, so the same `.so` files
load.

### Loading (in the worker, via `libloading`)

For language `L` requested for the first time:

1. Find `parser/L.{so|dylib|dll}` under the data dir.
2. `libloading::Library::new(path)`; look up the C symbol `tree_sitter_L`
   (`unsafe extern "C" fn() -> *const TSLanguage`).
3. Build a `tree_sitter::Language` from that pointer and keep the `Library`
   alive alongside it (the language borrows the loaded code).
4. Read `queries/L/highlights.scm` (+ `injections.scm` if present), build a
   `HighlightConfiguration`, and `.configure(&GROUPS)` against the canonical
   highlight-group list (below).
5. Cache `{Library, HighlightConfiguration}` keyed by `L`.

All of this is inside `catch_unwind`; a bad library that *panics* yields a
`ts_error` and no spans, while one that *segfaults* is handled by respawn +
per-language strike-out. (Exact tree-sitter constructors — `Language` from a raw
pointer, `HighlightConfiguration::new` arg order — are pinned during
implementation against `tree-sitter` 0.26; the loader pattern follows helix /
`tree-sitter-loader`.)

### Filetype detection

The **server** maps the buffer's path extension to a language name with a small
built-in table (`.rs`→`rust`, `.py`→`python`, `.json`→`json`, `.toml`→`toml`,
`.md`→`markdown`, `.js`→`javascript`, …) and sends that name to the worker. A
buffer with no path, an unknown extension, or no installed parser simply gets no
highlights — plain text, today's behavior. (`:set filetype=` override: future.)

---

## Protocol

### Worker RPC (server → worker), reusing `nxvim-rpc` framing

- **`ts_open`** (notification, server→worker):
  `{ buffer: u64, tick: u64, language: str, text: str, first_line, last_line }`
  — (re)initialize a buffer: set the shadow text to `text`, do the **initial full
  parse**, and reply with highlights for the visible range. Sent on first sync and
  on every **resync** (undo/redo, `:e` reload — anything that replaces the whole
  rope). The one unavoidable full-text send; it happens once per open, async.
- **`ts_edit`** (notification, server→worker):
  `{ buffer: u64, tick: u64, edits: [Edit…], first_line, last_line }` where each
  `Edit` is
  `{ start_byte, old_end_byte, new_end_byte, start_row, start_col, old_end_row, old_end_col, new_end_row, new_end_col, text }`
  — apply the deltas to the shadow buffer (`remove [start_byte,old_end_byte)`,
  `insert text` at `start_byte`), feed the matching `InputEdit` to the tree,
  **reparse incrementally**, and reply with highlights for the visible range.
  `text` is the inserted bytes only (empty for a pure deletion); the positions are
  tree-sitter `Point`s (row + byte-column) computed by the editor.
- **`ts_view`** (notification, server→worker):
  `{ buffer: u64, first_line, last_line }` — viewport moved, no text change;
  re-run the query over the new range against the current tree, reply with spans.
- **`ts_highlights`** (notification, worker→server):
  `{ buffer: u64, tick: u64, first_line, last_line, spans: [[line, start_byte, end_byte, group], …] }`
  — `line` is an absolute buffer line; `start_byte`/`end_byte` are byte columns
  **within that line**; `group` is the canonical highlight-group **name string**
  (e.g. `"keyword"`, `"string"`, `"function.call"`). Absolute-line keying makes
  the result robust to scrolling between request and reply.
- **`ts_error`** (notification, worker→server): `{ buffer, language, message }`
  — load/query failure for a language; the server stops requesting it and may
  surface the message once.

The worker keeps, per buffer id, `{ shadow: Rope, tree: Tree, language }`. A
`ts_edit` for a buffer it has never seen (or whose language it lacks) is ignored
until the next `ts_open` — the server always opens before editing.

Group names cross the wire as strings, so neither side needs a shared enum and
the canonical list can grow without lockstep changes — consistent with how the
existing `redraw` map uses ad-hoc msgpack keys rather than a shared type.

### Client redraw (`redraw` map, server → TUI) — new key

The server adds **`highlights`** to the existing `redraw` map: a per-visible-row
array (aligned with `lines`), each element an array of
`[start_col, end_col, group]` spans in **screen columns** (tab- and wide-char
aware, like `selection`), or `Nil`/empty for an unhighlighted row.

The server converts the worker's *byte* columns to *screen* columns with
`nxvim_core::unicode::virtcol` (the exact function `selection_spans` already uses)
against the current visible line text — so highlight spans line up with painted
glyphs the same way the selection does. **This conversion is the server's job**,
keeping the TUI screen-column-only and gutter-agnostic.

> Design choice: highlights are **not** added to `nxvim-core`'s `View`. They are
> produced by the out-of-core worker and merged into the redraw map by the server
> (which already builds that map by hand). Core stays pure; the only core change
> is `changedtick`.

---

## Core change: an edit journal on `Buffer`

Incremental sync needs the *deltas*, not just a "changed" bit. `Buffer` becomes
the single choke point for mutation and records each one. It gains:

```rust
pub changedtick: u64,        // bumped per mutation (neovim's b:changedtick)
edits: Vec<BufferEdit>,      // journal since the last drain
resync: bool,                // whole-rope replacement happened (undo / reload)
```

where

```rust
pub struct BufferEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,            // == start_byte for a pure insert
    pub new_end_byte: usize,            // == start_byte for a pure delete
    pub start_point: (usize, usize),    // (row, byte-col) before the edit
    pub old_end_point: (usize, usize),  // before the edit
    pub new_end_point: (usize, usize),  // after the edit
    pub text: String,                   // inserted bytes ("" for a deletion)
}
```

**Mutation routing.** Today `editor.rs` pokes `self.buffer.text.insert/remove`
directly in ~14 places. Those become two tracked methods —
`Buffer::insert(byte, &str)` and `Buffer::remove(range)` (with
`insert_char` on top) — that mutate the rope, compute the `BufferEdit` (positions
read from the rope before/after), push it to the journal, and bump `changedtick`.
`normalize()`'s trailing-newline insertion routes through `insert` too, so the
shadow stays byte-identical including the phantom `\n`. `text` stays `pub` for
*reads*; only writes funnel through the methods. This is a mechanical refactor and
arguably better structure regardless of highlighting (one place owns mutation).

**Resync.** Undo/redo swap the whole rope (`self.buffer.text = snap.text`); `:e`
loads a new buffer. Diffing those into deltas isn't worth it — they set
`resync = true` instead. The server, seeing `resync`, discards the journal and
sends a full `ts_open`. Initial buffer creation is likewise an open.

**Server side.** The server keeps a `last_sent_tick` per buffer. In `redraw()`:
if `resync` (or never-opened) → drain + `ts_open(full text)`; else if the journal
is non-empty → drain + `ts_edit(deltas)`; else if only the viewport moved →
`ts_view`. `Buffer` exposes `take_edits() -> (Vec<BufferEdit>, bool resync)` and
`changedtick`. That, plus the existing `lines()`/`line()` for text, is the entire
core surface the server needs.

---

## TUI: group → color, and painting

The TUI gains a **theme**: a `&str` group-name → `ratatui::style::Style` map with
a sensible dark-terminal default (ANSI/indexed colors so it works on any
terminal). The canonical groups and rough default mapping:

| group                                            | default style          |
| ------------------------------------------------ | ---------------------- |
| `keyword`, `keyword.*`, `conditional`, `repeat`  | magenta                |
| `function`, `function.call`, `function.builtin`  | blue                   |
| `type`, `type.builtin`, `constructor`            | yellow                 |
| `string`, `string.*`, `character`                | green                  |
| `number`, `boolean`, `constant`, `constant.*`    | cyan                   |
| `comment`                                        | dim / gray             |
| `variable`, `property`, `parameter`              | default fg             |
| `operator`, `punctuation.*`                      | default fg (subtle)    |
| `attribute`, `label`                             | cyan                   |
| (unknown)                                        | default fg             |

Painting changes are localized to `render_text`/`highlight_line`. Today that
function expands tabs and applies one `REVERSED` style for selection. It now also
walks each cell's screen column to find the covering highlight span and applies
its group's `Style`; the visual **selection composes on top** (syntax sets `fg`,
selection adds the `REVERSED` modifier). Runs of identical style coalesce into
ratatui `Span`s. The number gutter and cursor positioning are untouched.

This keeps the established split intact: the **worker/server decide which cells
are which group; the client decides what each group looks like.**

---

## Server loop change

`run()`'s `while incoming.recv()` becomes a `tokio::select!` over **two** RPC
inboxes — the client's and the worker's — plus the existing `:sleep` await:

- a **client** message → handle as today, then `redraw()` (which may fire a
  `ts_highlight`);
- a **worker** `ts_highlights` → store spans for that tick, then `redraw()` so
  the client repaints with color;
- a **worker** death (inbox closed) → `SyntaxClient` respawns per the backoff
  rules; no redraw needed.

The `SyntaxClient` is created lazily on first attach (no UI, no worker). Spans are
cached on the server keyed by buffer + tick; `redraw()` selects the
best-available spans for the currently visible lines and converts them to screen
columns.

---

## Edge cases

- **No parser installed / unknown filetype / no path** → no `highlights` key;
  plain text. Exactly today's rendering.
- **Worker crash (segfault or panic-to-abort)** → respawn with backoff; editing
  never pauses; highlighting resumes on the next request. Crash-loop → circuit
  breaker; poison grammar → per-language strike-out.
- **Huge file / slow parse** → redraws stay immediate with stale/empty spans;
  color catches up. The whole-text send is debounced to one in-flight request.
- **Tabs / wide chars** → byte→screen-column conversion via `virtcol`, so spans
  align with glyphs (same path as selection).
- **Selection over syntax** → compose: syntax `fg` + selection `REVERSED`.
- **Scroll animation** → the destination redraw is highlighted; the over-scan
  band shown *mid-slide* may be unhighlighted in v1 (snaps to color on settle).
  Noted as a follow-up.
- **Stale tick** → results are tagged with the tick parsed; the server prefers
  the latest received spans. Brief staleness during fast edits is acceptable and
  self-correcting.

---

## Testing (black-box, per the project's no-unit-test rule)

Everything is exercised through the running server / painted screen. Highlighting
is asynchronous, so screen/redraw assertions **poll redraws until one carries
`highlights`** (bounded wait), rather than relying on a single barrier.

A test **fixture grammar** makes this hermetic without a network: a test helper
(or `build.rs`) compiles a known grammar's bundled C sources into a
`parser/<lang>.{so,dylib}` and writes its `highlights.scm` into a temp
`queries/<lang>/`, then points the server at it via `NXVIM_DATA_DIR`. Rust is the
natural fixture (dogfoods the repo).

- **RPC/`View` tier** (`nxvim-server/tests`): open a `.rs` buffer with known
  content; wait for a redraw with `highlights`; assert the `fn`/`let` keyword
  ranges, a string literal, and a comment carry the expected group names, in
  correct screen columns (including a line with a leading tab to prove
  byte→screen conversion).
- **Crash-resilience test:** a reserved language name (e.g. `"__crash"`) whose
  worker handler calls `std::process::abort()`. Request it, then assert the
  editor **still edits** (`i...<Esc>` changes the buffer) and that highlighting
  for a *good* language recovers afterward — proving isolation and respawn.
- **Tier 2 screen test** (`nxvim/tests/screen.rs`): open a `.rs` buffer, wait for
  the highlighted redraw, assert a keyword cell carries the theme's keyword
  `fg` color in the painted ratatui buffer, and that a selected keyword cell is
  both colored and `REVERSED`.
- **No-grammar test:** open a `.txt`/no-path buffer; assert redraws never carry
  `highlights` and text paints plain (the path stays exactly as today).
- **Incremental / huge-file test:** generate a large `.rs` buffer (tens of
  thousands of lines), type a character, and assert (a) the buffer/redraw round
  trip stays fast (no per-keystroke full-text work on the editor side), and
  (b) the `ts_edit` the server emits carries a **single tiny delta**
  (`text.len()` ≈ 1, byte range ≈ 1), proving sync cost scales with the edit, not
  the file. Exercised at the protocol layer via a test seam that captures the
  last worker message — the deterministic, non-timing assertion that "it works on
  huge files."

The wall-clock timing of catch-up is **not** asserted (client/async), matching the
coverage boundary the smooth-scrolling design set.

---

## Dependencies (pinned `=x.y.z`, latest stable)

Added under `nxvim-ts` only (plus `tokio` `process` feature for the server):

- `tree-sitter = "=0.26.9"` — core parser; `Tree::edit`, `parse_with`, `Query`,
  `QueryCursor::set_byte_range`. The **low-level** API (not `tree-sitter-highlight`)
  is required: the worker owns its `Tree` to reparse incrementally, whereas the
  highlight crate always reparses from scratch.
- `streaming-iterator = "=0.1.9"` — `QueryCursor::captures` yields a
  `StreamingIterator`; this provides the trait to drain it.
- `libloading = "=0.8.9"` — dynamic grammar load.
- `ropey` (workspace) — the worker's per-buffer shadow text, edited by delta and
  read by tree-sitter's `parse_with` callback (chunk-at-byte).

The highlights query is run directly with a `QueryCursor` restricted to the
visible byte range (`set_byte_range`); overlapping captures are resolved
**most-specific-wins** (broader spans written first, narrower overwrite) into
per-line byte-column spans.

No grammar crates are linked — grammars are loaded from disk at runtime, per the
installable-grammar decision.

---

## Implementation phases (for the follow-up plan)

1. **Core edit journal:** `Buffer::changedtick`, `BufferEdit`, `insert`/`remove`/
   `insert_char` tracked methods, `take_edits()`, `resync`; route `editor.rs`'s
   mutations (and `normalize`, undo/redo, `:e`) through them. Existing editing
   tests stay green — this is a pure refactor of mutation plumbing.
2. **`nxvim-ts` worker skeleton + protocol:** `run_worker`, data-dir resolver,
   `ts_open`/`ts_edit`/`ts_view`/`ts_highlights`/`ts_error` over `nxvim-rpc`;
   per-buffer shadow `Rope`; stub that echoes no spans. Binary routes
   `--__ts-worker`.
3. **Dynamic loader + incremental engine:** `libloading` grammar load, query
   build, the shadow-rope `parse_with` callback, `Tree::edit` + incremental
   reparse, span extraction over a line range, the `GROUPS` list, `catch_unwind`
   guards.
4. **`SyntaxClient` + server loop:** spawn/supervise/respawn + circuit breaker,
   per-buffer `last_sent_tick`, open/edit/view decision, debounce, span cache,
   byte→screen conversion, `highlights` in the redraw map, the `select!` loop.
5. **TUI theme + painting:** group→color map, `highlight_line` composition with
   selection.
6. **Tests:** fixture-grammar harness, the tiers above **plus a huge-file
   responsiveness test** (open a large generated `.rs`, assert edits stay
   instant and the `ts_edit` payloads are delta-sized, not file-sized), and
   whole-workspace `fmt`/`clippy`/`test` green.
