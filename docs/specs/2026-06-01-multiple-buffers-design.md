# Multiple open buffers — phased design

Status: **Phases 1–4 implemented** (Phase 5 is optional UI, deferred). This is
the implementation plan for the next major feature: supporting **multiple open
buffers** (the editor holding several files at once and switching between them),
the prerequisite for windows/splits/tabs later. See [*Buffers*](../architecture.md#buffers)
in the architecture doc for the shipped design.

Scope is deliberately **buffers only**, not windows. There is still exactly one
window onto one "current" buffer at any time. We add a buffer *list*, the
ability to *switch* the current buffer (preserving each buffer's content, undo,
modified state, and last cursor position), and the vim ex-command + RPC surface
to manage that list. Splits, the window layout tree, and a tabline remain future
work.

Read [`docs/architecture.md`](../architecture.md) first. The relevant code is
`crates/bemtvi-core/src/editor.rs` (the state machine and the single embedded
`Buffer`), `crates/bemtvi-core/src/buffer.rs` (the rope text type + edit
journal), `crates/bemtvi-core/src/view.rs` (the renderable `View`), and
`crates/bemtvi-server/src/lib.rs` (the RPC surface + the syntax integration,
which currently hardcodes `BUFFER_ID = 0`).

---

## Design model

Today `Editor` mixes two concerns that vim keeps separate:

- **Buffer state** (the "file"): rope `text`, `path`, `modified`, `changedtick`,
  the `BufferEdit` journal, and — currently misplaced on `Editor` — the
  `undo_stack` / `redo_stack`. In vim, undo history is *per buffer*.
- **Window state** (the "view"): `cursor`, `top` (scroll), `mode`, `desired_col`
  / `desired_eol`, `visual_anchor`, and the transient pending-input fields.

The plan separates them like this:

```
Editor
├── buffers: BufferStore           // id -> OpenBuffer, monotonic 1-based ids
│     └── OpenBuffer
│           ├── buffer: Buffer       // existing buffer.rs type (text + journal)
│           ├── undo_stack / redo_stack
│           └── saved_cursor, saved_top   // window pos while NOT current
├── current: BufferId              // which buffer the window shows
├── alternate: Option<BufferId>    // vim's `#` (the `<C-^>` target)
│
├── cursor, top, mode, desired_col, … // WINDOW state — stays on Editor
├── register                          // GLOBAL (unnamed register is shared)
└── options                           // GLOBAL for now (buffer-local = future)
```

Key decisions, each with a rationale a future maintainer can check against vim:

- **`BufferId(u64)`**, monotonic, **1-based**, never reused. Buffer 1 is the
  first file (or the initial `[No Name]`). Matches vim's buffer numbers.
- **There is always ≥ 1 buffer.** Deleting the last buffer leaves a fresh
  `[No Name]` buffer (vim does this), so `current` is always valid.
- **`Buffer` (buffer.rs) stays the pure text type.** Undo and saved window
  position live in a new `OpenBuffer` wrapper in `editor.rs`, so `bemtvi-core`'s
  text model stays unchanged and `buffer.rs` keeps its single mutation/journal
  responsibility.
- **`self.buffer` field → `self.buffer()` / `self.buffer_mut()` accessors** that
  resolve the current `OpenBuffer`'s inner `Buffer`. External readers
  (`view.rs`, `server/lib.rs`) change `ed.buffer.foo` → `ed.buffer().foo`. This
  is the single largest mechanical churn and is isolated to Phase 1.
- **Register and options stay global** on `Editor`. The unnamed register being
  shared across buffers matches vim (yank in A, paste in B works). Buffer-local
  options are explicitly out of scope; note it as future work.
- **Switching marks the incoming buffer for syntax resync.** Through Phase 3 the
  server keeps its *single* `SyntaxState` and simply re-opens the worker with
  the current buffer's text on every switch — correct, just not optimal. Phase 4
  makes the worker state per-buffer so switching back is instant.

`changedtick`, `modified`, the edit journal, and `mark_resync()` are already on
`Buffer`, so they become per-buffer "for free" once buffers multiply.

---

## Phase 1 — Refactor: buffer store with a single buffer (no behavior change)

**Goal:** introduce the data model above with the store holding exactly one
buffer. Pure refactor — every existing test passes unchanged, no new user-facing
behavior.

**Changes (`crates/bemtvi-core/src/editor.rs`):**

- Add `BufferId(u64)` (public; the RPC layer and tests will name buffers by it).
- Add `OpenBuffer { buffer: Buffer, undo_stack, redo_stack, saved_cursor,
  saved_top }`. Move the `Snapshot` undo type and the `undo_stack`/`redo_stack`
  fields off `Editor` into `OpenBuffer`. `saved_cursor`/`saved_top` are unused in
  Phase 1 (wired in Phase 2).
- Add `BufferStore { map: BTreeMap<BufferId, OpenBuffer>, next_id: u64 }` with
  `insert`, `get`/`get_mut`, `ids()` (sorted), `contains`. (A `BTreeMap` keeps
  `:bnext` ordering and `:ls` output stable and id-sorted like vim.)
- `Editor` gains `buffers: BufferStore`, `current: BufferId`,
  `alternate: Option<BufferId>`; loses `buffer`, `undo_stack`, `redo_stack`.
- `Editor::with_buffer` seeds the store with one `OpenBuffer` (id 1) and sets
  `current`.
- Add private accessors: `cur(&self) -> &OpenBuffer`, `cur_mut`, and the public
  `buffer(&self) -> &Buffer` / `buffer_mut(&mut self) -> &mut Buffer` delegating
  to the current `OpenBuffer`. Rewrite `push_undo`/`undo`/`redo` to use
  `self.cur_mut().undo_stack` etc.

**Changes (`view.rs`, `server/lib.rs`):** mechanical rename of every
`ed.buffer.` / `self.editor.buffer.` to `ed.buffer().` / `self.editor.buffer().`
(and `…buffer_mut()` where `&mut` is needed, e.g. `take_edits`).

**Tests:** none added; the whole existing `editing.rs` suite is the regression
gate. Done when `cargo test --workspace` is green and `cargo clippy
--all-targets --all-features -- -D warnings` is clean.

**Handoff note for Phase 2:** the store, ids, alternate slot, and the
`saved_cursor`/`saved_top` fields now exist but only one buffer is ever created
and nothing switches. Phase 2 turns on creation and switching.

---

## Phase 2 — Multiple buffers + the switch mechanism

**Goal:** be able to open several files into distinct buffers and switch the
current buffer, with each buffer remembering its content, undo, modified flag,
and last cursor/scroll position. Observable via `:e` reuse and the alternate
buffer.

**Changes (`editor.rs`):**

- `fn add_buffer(&mut self, buffer: Buffer) -> BufferId` — allocates the next id,
  inserts an `OpenBuffer` (empty undo, `saved_cursor`/`saved_top` defaulted).
- `fn switch_buffer(&mut self, id: BufferId)`:
  1. stash `self.cursor`/`self.top` into the *outgoing* `OpenBuffer`'s
     `saved_cursor`/`saved_top`;
  2. set `alternate = Some(old current)`, `current = id`;
  3. restore `self.cursor`/`self.top` from the incoming buffer (clamp to its
     line count);
  4. leave any visual/operator-pending state (`reset_pending`, drop visual mode)
     and clear `message`;
  5. call `self.buffer_mut().mark_resync()` so the syntax worker re-syncs the new
     content, and `ensure_visible()`.
- `fn find_buffer_by_path(&self, path) -> Option<BufferId>` — for `:e` reuse
  (compare canonicalized paths; fall back to as-typed if canonicalize fails).
- Rework `ex_edit` (`:e`/`:edit`): if a buffer with that path already exists,
  `switch_buffer` to it; otherwise `add_buffer(Buffer::from_file(path))` then
  switch. The old in-place replacement is removed. Keep the `E37` modified-guard
  and `:e!` reload (reload = re-read into the *current* buffer when the path
  matches, preserving the buffer id).
- Add `:enew` / `:ene` — `add_buffer(Buffer::empty())` then switch.
- Add the alternate-buffer toggle: in `handle_normal_command`, `<C-^>` /
  `<C-6>` (`key.ctrl` + `Char('^')` / `Char('6')`) → `switch_buffer(alternate)`
  if set, else the `E23: No alternate file` message.

**Syntax (no code change needed yet):** because `switch_buffer` marks resync and
the server only ever drives the *current* buffer through its single
`SyntaxState`, highlighting stays correct across switches (it re-parses on each
switch). Phase 4 optimizes this.

**Tests** (`crates/bemtvi-server/tests/buffers.rs`, new file; mirror `editing.rs`
helpers):

- `:e a.txt`, edit, `:e b.txt`, `:e a.txt` → buffer A's text and **cursor
  position** are restored; B is untouched.
- `<C-^>` toggles between the two most-recent buffers.
- Undo is independent: edit A, switch to B, `u` in B does not touch A; switch
  back, `u` undoes A's edit.
- `:e` on an already-open path does **not** create a second buffer (verified in
  Phase 3 once `:ls` exists; for now assert cursor restore as the proxy).

**Handoff note for Phase 3:** switching works but the only ways to trigger it are
`:e` and `<C-^>`. Phase 3 adds the full navigation surface and the RPC API, plus
the ability to *observe* the buffer list directly.

---

## Phase 3 — Buffer-list ex-commands + RPC buffer API

**Goal:** the full user/observer surface for the buffer list.

**Ex-commands (`editor.rs`, `execute_ex` match):**

- `:ls` / `:buffers` / `:files` — build the listing into `self.message`, one
  line per buffer sorted by id: `"{id}{flags} \"{name}\" line {n}"`, where flags
  are `%` (current), `#` (alternate), `+` (modified), `a`/`h` (active/hidden).
  Match vim's column shape closely enough to be recognizable.
- `:b[uffer] {N|name}` — switch by id or by (sub)string path match; `E94`/`E86`
  on miss, `E93` on ambiguous name.
- `:bn[ext]` / `:bp[revious]` (a.k.a. `:bN`) / `:bf[irst]` / `:bl[ast]` —
  navigate the id-sorted list (with count support and wraparound for next/prev).
- `:bd[elete]` / `:bw[ipeout]` `{N?}` — remove a buffer from the store (default
  current). Block on unsaved changes without `!` (`E89`). If deleting the
  current buffer, switch to the alternate (else the previous/next id); if it was
  the last buffer, `add_buffer(Buffer::empty())` and switch so the invariant
  "≥ 1 buffer" holds. Emit a `ts_close` request in Phase 4.

**Quit/write semantics (extend existing `ex_*`):**

- `:wa` / `:wall` — write **every** modified buffer that has a path (currently
  writes only the one). Iterate the store; report count written.
- `:qa` / `:qall` — refuse with `E37` if **any** buffer is modified (without
  `!`); else quit.
- `:wqa` / `:xa` — write all, then quit.
- `:q` is unchanged: with one window it quits the editor. Document that closing a
  window-per-buffer is a *windows* feature, out of scope here.

**RPC surface (`server/lib.rs`, `dispatch`):**

- `nvim_list_bufs` → array of buffer ids.
- `nvim_get_current_buf` → current id.
- `nvim_set_current_buf` → `editor.switch_buffer(id)` (+ run pending).
- `nvim_buf_get_name` → that buffer's path (or `""`).
- `nvim_create_buf` → `editor.add_buffer(Buffer::empty())`, return the id
  (does not switch, matching neovim's `listed/scratch` create).
- **Honor the buffer-handle arg in `nvim_buf_get_lines`**: `params[0] == 0` means
  current (today's behavior); a non-zero id reads *that* buffer. Add an
  `Editor::lines_of(id)` helper. This is what lets tests read a non-current
  buffer's contents directly.

**Tests** (extend `buffers.rs`): `:ls` lists all opens with correct `%`/`#`/`+`
flags; `:bn`/`:bp` wrap and respect counts; `:bd` on current falls back to
alternate and blocks on unsaved without `!`; `:bd` of the last buffer yields a
fresh `[No Name]`; `:wa` writes every dirty buffer (assert bytes on disk for
two files); `nvim_list_bufs` / `nvim_buf_get_lines <id>` read a non-current
buffer.

**Handoff note for Phase 4:** functionally complete. The remaining work is making
syntax highlighting *efficient and per-buffer* instead of resync-on-every-switch.

---

## Phase 4 — Per-buffer syntax state in the server

**Goal:** the syntax worker tracks each buffer independently, so switching back
to a buffer shows its cached highlights immediately with no re-parse, and edits
to the current buffer send incremental deltas keyed by the right buffer id.

The worker protocol **already** takes a buffer id in `open`/`edit`/`view` and
tags `ts_highlights` replies — so this is server-side bookkeeping, not a protocol
change.

**Changes (`server/lib.rs`):**

- Replace the single `syntax_state: SyntaxState` and the `BUFFER_ID = 0` constant
  with `syntax_states: HashMap<BufferId, SyntaxState>`.
- `sync_syntax` operates on `editor.current`: look up (or default-insert) that
  buffer's `SyntaxState`, and pass the real `BufferId` (as `u64`) to
  `self.syntax.open/edit/view`.
- `store_spans` / `on_syntax_event` route the reply by the buffer id carried in
  the `ts_highlights` params into the matching `SyntaxState`.
- `highlights_for` reads spans from the **current** buffer's `SyntaxState`.
- On `:bd`/wipeout, send a `ts_close(id)` (add the message to `SyntaxClient`)
  and drop the `SyntaxState`, so the worker frees that tree. On worker
  `Restarted`, clear *all* states' `opened`/`pending`/`spans`.
- `switch_buffer` no longer *needs* to force a resync for correctness (the cache
  is retained per buffer), but keep marking resync only when the buffer content
  actually changed out of band.

**Tests:** mostly covered by existing screen/highlight tests staying green across
a buffer switch. Add (Tier 2, `crates/bemtvi/tests/screen.rs`) a switch-and-back
that asserts highlighted cells reappear without an intervening blank frame, and a
debug-only `__crash`-language test that a crash in one buffer's parse doesn't
corrupt another buffer's cached spans.

---

## Phase 5 (optional / stretch) — UI buffer indicator

Not required for "multiple open buffers," listed so it isn't forgotten:

- A minimal status-line buffer indicator (`[N/M]`) — small `View` field +
  client paint.
- A **tabline / bufferline** is a larger UI feature (its own `View` region and
  client widget) and should be its own spec, alongside actual windows/splits.

Keep the core feature (Phases 1–4) shippable without any of this.

---

## Invariants & gotchas to preserve

- **Always ≥ 1 buffer**; `current` always resolves. `:bd` of the last buffer
  creates a `[No Name]`.
- **Buffer ids are monotonic and never reused** (1-based). A wiped id stays gone.
- **Undo, `modified`, `changedtick`, and the edit journal are per buffer** — they
  already live on `Buffer`/`OpenBuffer`, so don't reintroduce any global undo.
- **Register and options are intentionally global** for now; buffer-local options
  are future work (note in `architecture.md` roadmap when Phase 3 lands).
- **`Buffer::byte_at` and the trailing-`\n` invariant are unchanged** — multiple
  buffers don't touch the text model; each buffer independently maintains it.
- **`switch_buffer` must clamp the restored cursor** to the incoming buffer's
  current line count (it may have shrunk via undo/edits while inactive).

## Roadmap doc update

When Phases 1–4 land, update `docs/architecture.md`: move "Multiple windows,
tabs, and buffers" out of *Not yet implemented* into a new *Buffers* subsection
(buffers done; windows/tabs still pending), and note buffer-local options as the
next gap.
