# In-process treesitter + treesitter indentation — design

**Status:** accepted; phases 1–3 implemented (worker deleted, highlighting now
in-process and synchronous), phases 4–5 (treesitter indentation) pending.
**Supersedes** the process-isolation architecture in
[`2026-06-01-syntax-highlighting-design.md`](2026-06-01-syntax-highlighting-design.md)
(highlighting *behavior* is unchanged for the user; only where the parser runs
changes). Folds and the `vim.treesitter` Lua API are explicitly **out of scope**
here — this doc unblocks them but does not build them.

## Why reverse the worker decision

The 2026-06-01 design runs tree-sitter in a **separate, crash-isolated child
process** and treats the link as *"fully asynchronous and advisory — the editor
never awaits it"* (that spec, §"Why a process…"). That premise is exactly right
for **highlighting**: colors are allowed to land a frame late, so async + stale
is fine, and a segfaulting grammar can't take the editor down.

The premise is **false for indentation.** Auto-indent on `Enter` / `o` / `O`,
and the `=` family of reindent operators, are *synchronous editing decisions*:
the cursor has to land in the right column the instant the key is pressed. A
value that arrives a frame later, applied as a follow-up edit, produces a visible
cursor jump and races with fast typing and undo. The same is true of the future
`vim.treesitter` Lua API, whose whole surface (`get_parser():parse()`,
`node:start()`, query `:iter_captures()`) is **synchronous, in-process** by
construction.

And here is the load-bearing point: **to compute treesitter indent at all, you
need a tree that is queryable synchronously, in-process. Getting one means
linking tree-sitter into the editor process — at which point the worker's crash
isolation is already forfeited.** Keeping the worker *and* adding an in-process
parser for indent is the worst case: two parsers, double the memory, the segfault
risk taken anyway. The worker is only a coherent design while tree-sitter stays
advisory-only forever. We want indent, so it doesn't.

So we go **in-process — the neovim way**: one tree per buffer, owned in the
editor process, queried synchronously for highlights *and* indent *and*
(eventually) the Lua API.

### The tradeoff we are accepting

| Property | Worker (today) | In-process (this doc) |
|---|---|---|
| Grammar segfault | editor survives, worker respawns | **editor process crashes** (like neovim) |
| Pathological parse stalls UI | impossible (async) | bounded by a **parse deadline** (see below) |
| Highlight latency | 1–2 frames behind typing | **same frame**, correct immediately |
| Treesitter indent / folds / `vim.treesitter` | impossible (async tree) | **possible** (sync tree) |
| Moving parts | worker binary mode, RPC wire, shadow-over-IPC, supervisor, circuit breaker | a library call |

We accept the segfault exposure because it is exactly neovim's posture, grammars
are user-installed and stable in practice, and a `catch_unwind` cannot catch a C
segfault anyway (the worker spec notes this). The **stall** risk — the only other
thing the process bought us — is retained in-process by wiring a parse deadline
(see *Stall safety* below), so a runaway parse degrades highlighting for one
frame instead of hanging the editor.

---

## Architecture after the migration

Today: `nxvim-server` holds a `SyntaxClient` that pipes
`ts_open`/`ts_edit`/`ts_view` to the `nxvim --__ts-worker` subprocess and ingests
`ts_highlights` notifications into a per-buffer span cache, projected at redraw.
`nxvim-server` has **no** compile-time tree-sitter dependency; only the `nxvim`
binary depends on `nxvim-ts`, purely to host the worker.

After: `nxvim-ts` becomes an ordinary **in-process library**. Its `Engine`
(incremental parse + query, `engine.rs`) and `Grammar` loader (`loader.rs`) move
over essentially **verbatim** — that logic is the valuable part and it survives.
What's deleted is the transport and supervision scaffolding wrapped around it.

The new ownership, chosen to avoid a split borrow between "the tree" and "the
buffer": **the editor owns the engine behind a trait object.** `nxvim-core`
defines a synchronous `SyntaxEngine` trait (an *interface* plus plain data — no
tree-sitter, no C, no I/O, so core stays pure per its invariant). `nxvim-ts`
implements it. The server constructs the concrete engine at startup and hands it
to the editor:

```text
            ┌────────────────────────── nxvim-server (one process) ───────────┐
  keypress  │   Server::input ─► Editor::input ──────────────┐                 │
 ──────────►│                       │ owns Buffer + cursor   │                 │
            │                       │ owns Box<dyn SyntaxEngine> ──► nxvim_ts  │
            │                       │   .edit(deltas)   (incremental reparse)  │
            │                       │   .indent(line)   (run indents.scm)      │
            │   Server::redraw ─────┴►.highlights(range)  (run highlights.scm) │
            └──────────────────────────────────────────────────────────────────┘
                       no subprocess, no RPC, no respawn
```

Dependency direction (no cycle): `nxvim-core` defines the trait + data types;
`nxvim-ts` depends on `nxvim-core` and tree-sitter; `nxvim-server` depends on
both. `nxvim-core` never gains a tree-sitter dependency.

### The `SyntaxEngine` seam (in `nxvim-core`)

```rust
// nxvim-core — interface + plain data only; keeps core pure and synchronous.
pub struct Span {            // one highlight span, buffer coordinates
    pub line: usize,
    pub start_byte: usize,   // byte column within the line
    pub end_byte: usize,
    pub group: String,       // capture name, e.g. "keyword"
}

pub struct IndentParams {    // the editor's effective indent settings
    pub shiftwidth: usize,   // resolved sw → ts → default
    pub tabstop: usize,
}

/// Synchronous, in-process syntax backend. The editor owns one and calls it
/// directly; a front end with none simply has no highlighting or ts-indent.
pub trait SyntaxEngine {
    fn open(&mut self, buffer: BufferId, language: &str, text: &str);
    fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]);   // incremental reparse
    fn close(&mut self, buffer: BufferId);
    fn highlights(&mut self, buffer: BufferId, first: usize, last: usize) -> Vec<Span>;
    /// Target indent **width in columns** for `line`, or `None` when there is no
    /// grammar / no `indents.scm` / the query is inconclusive (caller falls back).
    fn indent(&mut self, buffer: BufferId, line: usize, p: &IndentParams) -> Option<usize>;
}
```

`Editor` gains `syntax: Option<Box<dyn SyntaxEngine>>` (None in a bare core
test; Some in the real server). Because the engine lives *inside* the editor and
keeps its **own shadow rope** (today's `Engine` already does), there is never a
borrow conflict with `self.buffers` — `edit()` takes deltas by value and
`indent()`/`highlights()` query the engine's own shadow.

> The shadow duplicates buffer text (~2× memory per open buffer) — the same cost
> paid across the IPC boundary today. A later optimization can parse against the
> editor's live rope and drop the shadow; v1 keeps it because today's `Engine`
> works unchanged and there is no borrow to fight.

### Keeping the engine current

Today the *server* drains `Buffer::take_edits()` each frame and ships deltas to
the worker. Now the **editor** drains its own journal into its engine, at the
points where currency is required:

- After any buffer mutation, before an `indent()`/`highlights()` query, the
  editor calls `self.syntax.edit(buf, &batch.edits)` (or `open` on
  `batch.resync` — undo/reload/file-change). This is a direct, synchronous call
  that runs an incremental reparse; no wire, no tick-coalescing, no "pending"
  state.
- `highlights()` is memoized per `(buffer, changedtick, viewport)` so a redraw
  that changed nothing re-projects the cached spans instead of re-running the
  query every frame. This is the **only** survivor of the old `SyntaxState` —
  slimmed to a memo key + span cache, with all the async `pending` / `opened` /
  coalescing fields gone.

---

## Highlighting after the change

`Server::redraw` calls `editor.highlights(buf, first, last)` (overscan range as
today) and projects the returned `Vec<Span>` exactly as `highlights_for` does
now: byte→screen-column via `unicode::virtcol`, capture→`style_id` via the
registry. The difference is that the spans are **correct in the same frame** —
no second "catch-up" redraw, no `SyntaxEvent`, no `store_spans`.

Notable consequence for tests: the highlight-specific async lag — the separate
`ts_highlights` redraw that arrives a frame later — **disappears**, so the
poll-until-spans-arrive dance in the syntax tests becomes a single synchronous
assertion. (The general client-harness redraw race documented elsewhere is a
*different* phenomenon and is unaffected.)

### Stall safety (replacing process isolation)

`Engine::reparse` already calls `parser.parse_with_options(.., .., None)` and its
comment anticipates a deadline. Wire that `None` to a `ParseOptions` carrying a
**budget** (a progress callback that cancels after N microseconds). On cancel,
`parse_with_options` returns `None`; the existing code already keeps the last
good tree in that case, so a runaway parse costs one frame of stale highlights
instead of a hang. This gives us most of the worker's "never stalls" guarantee
without a process.

---

## Treesitter indentation (the new capability)

### Query loading

Extend `Grammar` to load an optional indent query alongside highlights:

```rust
pub struct Grammar {
    pub language: Language,
    pub query: Query,                 // highlights.scm (required)
    pub indents: Option<Query>,       // indents.scm  (optional)
    _lib: libloading::Library,
}
```

`loader.rs` reads `queries/<lang>/indents.scm` (neovim/nvim-treesitter's name —
plural, matching `highlights.scm` / `folds.scm` / `injections.scm`) and compiles
it if present; a missing file is not an error (the language just has no
ts-indent). This is **drop-in compatible** with an existing nvim-treesitter
`queries/` tree — the `.scm` files are reused as pure data; we never run
nvim-treesitter's Lua.

### The indent algorithm (ported to Rust in `nxvim-ts`)

The `.scm` files are data; the algorithm that interprets them is nvim-treesitter's
`indent.lua`, ported to Rust as `Engine::indent`. It evaluates the standard
indent captures over the node at the start of the target line, walking ancestors
and accumulating a level:

- `@indent.begin` — node opens one indent level for lines inside it.
- `@indent.end` / `@indent.dedent` — close / reduce a level.
- `@indent.branch` — lines like `else`/`case` dedent relative to their opener.
- `@indent.align` — align to a delimiter (e.g. multiline call args) with
  `#set!` directives (`indent.open_delimiter` / `indent.close_delimiter`).
- `@indent.zero` — force column 0 (e.g. C preprocessor).
- `@indent.ignore` / `@indent.auto` — skip / defer to fallback.

Output: `level * shiftwidth` → a **target column** (visual width). The engine
returns *width*, not literal whitespace — it decides "how deep", the editor
decides "how to spell it" (tabs vs spaces) using `IndentParams` + the buffer's
`expandtab`. `None` is returned when there is no grammar, no `indents.scm`, or
the query gives no verdict, so the caller can fall back.

Porting `indent.lua` faithfully is the bulk of the new code; it is well-specified
and the `.scm` corpus is the public nvim-treesitter one. Scope v1 to the core
captures above; `#set!` align directives and injected languages can follow.

### Editor hooks (in `nxvim-core`)

A single helper centralizes the policy and the fallback chain:

```rust
/// Indent width for a (possibly not-yet-filled) line. Treesitter first, then a
/// simple "copy previous non-blank line's indent" autoindent, then 0.
fn indent_for(&mut self, line: usize) -> usize {
    let p = IndentParams { shiftwidth: self.effective_shiftwidth(),
                           tabstop: self.buffer().options.tabstop };
    self.syntax.as_mut()
        .and_then(|s| s.indent(self.current_buffer_id(), line, &p))
        .or_else(|| self.autoindent_copy_prev(line))   // graceful, grammar-free
        .unwrap_or(0)
}
```

Wire it into the three sites that currently hardcode `cursor.col = 0`:

1. **`open_line` (`editor.rs:4256`, the `o`/`O` path)** — after inserting `\n`
   and `normalize()`, forward the edit to the engine, compute `indent_for(new_line)`,
   insert that much leading whitespace (rendered via existing `<Tab>`/`expandtab`
   logic), set the cursor past it.
2. **`Enter` in insert mode (`editor.rs:4339`)** — same: insert `\n`, sync engine,
   indent the new line, place the cursor.
3. **The `=` family** — `==`, `=` over a motion/visual, `gg=G`: for each line in
   range call `indent_for(line)` and rewrite that line's leading whitespace. This
   is now a plain synchronous loop; no async to fight.

Order is load-bearing: the `\n` (and any auto-inserted whitespace) must be
`edit()`-forwarded to the engine *before* `indent()` is queried, so the tree
reflects the line being indented.

Backspace-over-autoindent and `indentkeys`-style re-triggers (e.g. typing `}`
dedenting the line) are v2; v1 delivers Enter/`o`/`O` and the `=` operators.

---

## What gets deleted / moved / added

**Deleted:**
- `crates/nxvim-server/src/syntax.rs` — `SyntaxClient`, the `supervise` /
  `run_worker_once` loop, the circuit breaker, `SyntaxEvent`.
- Worker mode in `crates/nxvim/src/main.rs` — `TS_WORKER_FLAG`, `run_ts_worker()`,
  and the `argv[1]` dispatch to it.
- `crates/nxvim-ts/src/lib.rs` worker loop (`run`, `handle`, the `ts_*` dispatch,
  `$NXVIM_TS_RECORD`) — replaced by a plain library root re-exporting `Engine`.
- `crates/nxvim-rpc/src/syntax.rs` wire types (`EditWire`/`SpanWire` encode/decode)
  — nothing crosses a process boundary now; the engine takes `&[BufferEdit]` and
  returns `Vec<Span>` directly. (Keep only if a field shape is still convenient
  internally; otherwise remove.)
- The async half of `crates/nxvim-server/src/treesitter.rs` — `on_syntax_event`,
  `store_spans`, `sync_syntax`'s pending/coalescing, the worker `open/edit/view`.
  `highlights_for`'s projection logic **stays** (now sourced from
  `editor.highlights()`), and a slim per-buffer memo replaces `SyntaxState`.

**Moved (≈verbatim):** `nxvim-ts/src/engine.rs` (`Engine`, incremental reparse,
`extract_spans`) and `nxvim-ts/src/loader.rs` (`Grammar`, dlopen, ABI probe,
`is_valid_language` traversal guard). The grammar data-dir layout and
`NXVIM_DATA_DIR` override are unchanged.

**Added:** `SyntaxEngine` trait + `Span`/`IndentParams` in `nxvim-core`;
`impl SyntaxEngine for Engine` in `nxvim-ts`; `Engine::indent` + `indents.scm`
loading; the editor `indent_for` helper and its three call sites; the parse
deadline.

**Cargo changes:**
- `nxvim-ts/Cargo.toml`: drop `rmpv`, `tokio`, `nxvim-rpc`; add `nxvim-core`.
  Keep `tree-sitter`, `ropey`, `libloading`, `streaming-iterator`, `anyhow`.
- `nxvim-server/Cargo.toml`: add `nxvim-ts` (first real tree-sitter dependency
  for the server).
- `nxvim/Cargo.toml`: drop the direct `nxvim-ts` dep (now transitive via server).
- Workspace deps unchanged (`tree-sitter = "=0.26.9"`, etc.).

---

## Edge cases

- **No grammar / no `indents.scm`** → `indent()` returns `None`; the editor falls
  back to copy-previous-line autoindent, then column 0. No loud failure here is
  correct: indentation is best-effort, not a missing feature (contrast the
  project's "no silent stubs" rule, which targets things that *pretend* to work —
  a `None` that visibly falls back is honest).
- **Grammar fails to load (bad ABI / missing symbol)** → loader returns `Err`;
  the editor echoes once ("treesitter: grammar 'X' failed to load") and proceeds
  un-highlighted, as today — but now in-process, so the message is synchronous.
- **Poison grammar segfault** → process dies. Accepted (see tradeoff table).
  Mitigation is upstream grammar quality + the ABI probe on load.
- **Huge file / pathological parse** → the parse deadline cancels; last good tree
  is retained; one frame of stale highlights, no hang.
- **Undo / file reload** → `take_edits().resync` triggers `engine.open` with full
  text instead of deltas (same trigger as today, moved into the editor).

---

## Testing (black-box, per the no-unit-test rule)

- **Highlighting** (`crates/nxvim/tests/syntax.rs`) — the fixture (compile
  `tree-sitter-rust` C into `parser/rust.so`, write `highlights.scm`) is reused,
  but `NXVIM_TS_WORKER` and the subprocess go away, and the **poll-until-spans**
  helper collapses to a single synchronous redraw assertion (highlights are
  same-frame now). Update accordingly.
- **Delete** the worker-only tests: `crates/nxvim-ts/tests/worker.rs` (RPC
  round-trips), the `"__crash"` crash-resilience test, and the `$NXVIM_TS_RECORD`
  tiny-delta test (the delta path is now an internal `Engine::edit` call; assert
  incrementality, if at all, as an in-process property).
- **New indent tests** (`crates/nxvim-server/tests/`, black-box via the server):
  point `NXVIM_DATA_DIR` at a fixture that adds `queries/rust/indents.scm`
  (vendored from nvim-treesitter), feed Rust source, press `o`/`Enter`/`==`/`gg=G`,
  assert `cursor`/`lines` show the expected indentation. Because everything is
  now synchronous, these are barrier tests, not polled ones.

---

## Implementation phases (for the follow-up plan)

1. **Library-ize `nxvim-ts`.** Strip the worker loop; expose `Engine` + `Grammar`
   as a plain lib. Add `nxvim-core` dep. (No behavior yet; it just compiles as a
   library.)
2. **`SyntaxEngine` trait in core + impl in `nxvim-ts`.** Define trait/`Span`/
   `IndentParams`; `impl SyntaxEngine for Engine` (indent stubbed to `None`).
3. **Cut the server over to in-process highlighting.** Editor owns the engine;
   server drains edits into it and queries `highlights()` at redraw; delete
   `syntax.rs`, the worker mode, the RPC wire, and the async half of
   `treesitter.rs`; add the per-buffer span memo. Wire the parse deadline. Update
   `syntax.rs` tests to synchronous. **Highlighting parity reached, worker gone.**
4. **`indents.scm` loading + the indent algorithm.** Extend `Grammar`; port
   `indent.lua`'s core captures to `Engine::indent`.
5. **Editor indent hooks.** `indent_for` + fallback; wire `open_line`, insert-mode
   `Enter`, and the `=` family. Add indent tests with the rust `indents.scm`
   fixture.
6. **(Later, now unblocked)** copy-previous-line nuances, `indentkeys` retriggers,
   `vim.treesitter` Lua API, treesitter folds.

Phases 1–3 are a behavior-preserving refactor (highlighting identical, worker
deleted); 4–5 deliver the actual feature the user asked for. Each phase is
independently testable and leaves the tree green.
