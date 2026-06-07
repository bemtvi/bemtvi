# Extmark / decoration layer — design

**Status:** accepted; not yet implemented. Scope is the **extmark layer only**:
namespaces + buffer-anchored extmarks carrying `hl_group`/`priority`, projected
into the redraw highlight payload. The **decoration-provider** model
(`nvim_set_decoration_provider`, the `on_win`/`on_line` redraw callbacks) and
`vim.treesitter.start`'s highlighter stay **explicitly deferred** and keep their
honest `_notimpl` (see *Non-goals*). This doc unblocks LSP semantic tokens and
plugin-driven highlights; it does not build them.

Builds directly on the projection pattern already shipped twice — treesitter
highlights ([`treesitter.rs::highlights_for`](../../crates/nxvim-server/src/treesitter.rs))
and LSP diagnostics ([`lsp/diagnostics.rs::diagnostics_for`](../../crates/nxvim-server/src/lsp/diagnostics.rs)).
An extmark layer is the *generalization* of those: a third highlight source that
rides the same `*_for(buffer, numbers, styles)` → `window_value` seam.

## Why extmarks, and why first

From the treesitter-Lua-platform design's non-goals: real
`vim.treesitter.start` "needs an extmark/decoration layer nxvim doesn't have,"
and LSP semantic tokens "share only the future highlight-layering primitive."
That primitive is the **extmark**: a buffer-anchored position (or range) that
shifts with edits and can carry a highlight group. Almost every plugin highlight
the user actually wants — LSP semantic tokens, gitsigns-style signs/blame,
diagnostics-as-marks, search-everywhere overlays — is an *extmark* consumer, not
a *decoration-provider* consumer.

The decoration-provider callbacks (`on_line` fired per visible row, every frame)
are a separate, hotter seam: they re-enter Lua during redraw, which the current
batched bridge (snapshot in → run → drain effects out) is not shaped for, and
they buy little for treesitter specifically because the Rust `Engine` already
highlights treesitter on the hot path. So extmarks land first as the foundation;
decoration providers are a later, clearly-delineated effort gated on a concrete
consumer.

## What an extmark is here

An extmark is identified by `(buffer, namespace, id)` and anchored to a byte
range in the buffer's rope. v1 carries only what the highlight layer needs:

```rust
// nxvim-core
pub struct Extmark {
    pub id: u64,
    pub start: usize,           // byte offset, anchored, shifts with edits
    pub end: Option<usize>,     // byte offset; None ⇒ a point mark (no span)
    pub hl_group: Option<String>,
    pub priority: u32,          // default 4096 (neovim's DEFAULT_PRIO)
}
```

Anchoring is **byte-offset based**, consistent with the rest of nxvim's text
model (architecture.md → *Text model*). neovim stores `(row, col)`; we store a
single byte offset and derive `(row, col)`/screen columns at projection time
exactly as `highlights_for`/`diagnostics_for` already do via
`unicode::virtcol`. This keeps anchor-shifting a one-dimensional arithmetic
problem (see below) instead of a 2-D row/col fixup.

Fields deliberately omitted in v1 (each fails loud or is silently dropped per
the rule below): `virt_text`, `virt_lines`, `sign_text`, `conceal`, `ephemeral`,
`hl_eol`, `right_gravity`/`end_right_gravity` (we fix gravity — see *Anchor
shifting*), `hl_mode`, `line_hl_group`, `cursorline_hl_group`. An option we do
not yet honor must **error at the call site**, never be quietly ignored, so a
plugin can't believe a feature works when it doesn't (CLAUDE.md → *No silent
stubs*).

## Storage

Authoritative state lives in **core**, owned per buffer, so every front end
shares identical behavior and `nxvim-core` stays pure/synchronous:

```rust
// per Buffer (or an editor-side side table keyed by BufferId — see "Open question")
pub struct ExtmarkStore {
    // namespace id -> (extmark id -> Extmark)
    by_ns: HashMap<u32, BTreeMap<u64, Extmark>>,
    next_id: HashMap<u32, u64>,   // per-namespace id allocator
}
```

Namespaces are a process-global registry (name → `u32`), mirroring
`nvim_create_namespace`'s "create-or-get by name, anonymous if name empty"
contract. Namespace `0` is reserved/global as in neovim. The registry lives on
the editor (not per buffer): the same namespace id addresses marks across all
buffers.

This also retires the known incomplete in
[`install.rs`](../../crates/nxvim-lua/src/install.rs) (the `nvim_set_hl` note
that "a non-zero `ns` is silently folded into the global namespace"): once
namespaces are real, `HlSet` can be keyed by ns. (Per-window/`nvim_set_hl`
namespacing is **not** in this doc's scope — we only make the namespace *ids*
real; folding `nvim_set_hl` onto them is a follow-up that this unblocks.)

## Anchor shifting (the load-bearing correctness piece)

Every buffer mutation already funnels through two choke points in
[`buffer.rs`](../../crates/nxvim-core/src/buffer.rs):

- `Buffer::record(edit)` — runs after every `insert`/`remove`, carrying a
  `BufferEdit { start_byte, old_end_byte, new_end_byte, .. }`. This is the
  single place edits are journaled and `changedtick` bumped.
- `Buffer::mark_resync()` — the whole rope was replaced (undo/redo, file
  reload).

Extmark anchors shift in `record`, by the same byte arithmetic an LSP/treesitter
edit uses. For an edit replacing `[start_byte, old_end_byte)` with new content
ending at `new_end_byte` (let `delta = new_end_byte - old_end_byte`, signed),
each stored offset `p` moves as:

| where `p` sits | new value | rationale |
|---|---|---|
| `p <= start_byte` | unchanged | before the edit |
| `p >= old_end_byte` | `p + delta` | after the edit, slides |
| `start_byte < p < old_end_byte` | clamp to `start_byte` | *inside* deleted text |

We fix gravity (neovim's default right-gravity for `start`, left for `end`); a
mark whose anchor is swallowed by a deletion collapses to the edit point rather
than vanishing — matching neovim's "marks survive, collapse to a point" feel. An
extmark whose `start == end` after shifting is still a valid point mark.

`mark_resync()` (rope wholesale-replaced) **clears all extmarks** in every
namespace for that buffer: the old byte offsets are meaningless against new
text, and unlike treesitter/LSP (which re-derive from the new full text) an
extmark has no source of truth to rebuild from. This matches neovim losing
extmarks on a destructive `:edit!`/reload. (A future optimization could map
through undo, but v1 clears — loud and correct beats clever and stale.)

Because shifting lives in core's choke point, it is exercised by *every* edit
path (normal-mode operators, insert mode, `:s`, macros) for free, and verified
end-to-end through the running server.

## Redraw projection

A new `Server::extmarks_for(buffer, numbers, styles) -> Value` mirrors
`highlights_for`/`diagnostics_for`:

1. For each visible row (`numbers`, 1-based, `None` = filler), find extmarks
   across all namespaces whose byte range intersects that buffer line.
2. Convert byte offsets to screen columns with `unicode::virtcol(&text, byte,
   tabstop)` — the same tab/wide-char projection highlights and diagnostics use,
   so colors line up with glyphs. A multi-line extmark contributes a span to
   each row it covers (start row: anchor→eol; middle rows: full; end row:
   bol→end), the standard range-highlight unrolling.
3. Resolve `hl_group` through `editor.highlights.resolve` / `resolve_capture`
   and `styles.intern(style)` → a per-frame style id, identical to the other two
   sources. A point mark (no `end`) with only `hl_group` contributes nothing
   visible in v1 (no `virt_text`/`sign`), so it is skipped at projection.

**Layering / priority.** Extmark highlights merge into the **same per-window
`highlights` array** as treesitter (they are fg/bg styling, unlike diagnostics'
underline array). Overlap resolution uses neovim's priority model: treesitter
highlights get baseline priority `100` (neovim's `TSHighlighter` default),
extmarks default to `4096`, so a semantic-token or plugin extmark wins over the
base treesitter color by default while leaving lower-priority marks below it.
The server emits the merged spans in **ascending priority order** so the
client's existing last-write-per-cell painting yields the top priority — *to be
confirmed against the TUI painter during Phase 3*; if the client does not paint
last-wins, the server resolves overlaps into non-overlapping spans before
emitting (no client change). Either way the wire shape of one entry is unchanged:
`[start_col, end_col, group, style_id]`.

**Memoization.** The expensive treesitter query stays memoized per
`(changedtick, viewport)` in `refresh_highlights`; extmark spans are read
**live** from the buffer's `ExtmarkStore` on every frame instead. The set per
line is small and always reflects the current marks (including a set/del/clear
that bumps no `changedtick`), so there is no cache to stale and no extra
invalidation tick to maintain — the merge only runs for lines that actually carry
a mark (the fast path is byte-identical to the pre-extmark projection).

## Lua API surface

Mutations ride the existing **effect-queue** (`Shared` → drained in
[`effects.rs`](../../crates/nxvim-server/src/effects.rs)); reads come from a
**snapshot mirror** refreshed before each Lua run (like `vim._bufs`). New
functions in [`install.rs`](../../crates/nxvim-lua/src/install.rs):

- `nvim_create_namespace(name) -> integer` — create-or-get; empty name ⇒ fresh
  anonymous ns. Resolved synchronously against the mirrored registry (and queued
  if newly created) so the id is stable within the run.
- `nvim_buf_set_extmark(buf, ns, row, col, opts) -> id` — `opts` reads
  `end_row`/`end_col`/`hl_group`/`priority`/`id`; **any other key errors**.
  Returns the id synchronously (see wrinkle below), queues an `ExtmarkOp::Set`.
- `nvim_buf_del_extmark(buf, ns, id) -> bool` — queues `ExtmarkOp::Del`; returns
  whether the id existed per the snapshot mirror.
- `nvim_buf_get_extmarks(buf, ns, start, end, opts) -> list` — pure read from
  the snapshot mirror; supports `details` (return the opts table) and `limit`.
- `nvim_buf_clear_namespace(buf, ns, line_start, line_end)` — queues
  `ExtmarkOp::Clear`.

### The synchronous-id wrinkle

`nvim_buf_set_extmark` must return the new id *immediately*, but mutations are
deferred until the effect drain. Resolution: the **id allocator is mirrored**.
Before each Lua run the server pushes the per-`(buffer, namespace)` `next_id`
into the snapshot; `set_extmark` allocates from (and increments) the Lua-side
mirror, returns that id, and queues `ExtmarkOp::Set { buffer, ns, id, .. }` with
the *resolved* id. The server applies with that exact id, keeping core
authoritative while the return value is correct and stable. Reads within the
same run see queued sets by also updating the Lua-side mirror on `set`/`del` (the
mirror is the run's working copy; the queue is the durable write) — the same
read-your-writes shape the buffer mirror already provides. An explicit
`opts.id` (caller-chosen) bypasses the allocator, as in neovim.

## Testing

Black-box through the running server, per CLAUDE.md (no unit tests):

- **Projection:** a Lua config creates a namespace and sets an extmark with an
  `hl_group`; assert the redraw `highlights` payload for that window contains the
  expected `[start_col, end_col, group, style_id]` span with the resolved style.
  (Harness: drive Lua as `treesitter_lua.rs` does; assert via the redraw helpers
  in `editing.rs`, taking the *latest* redraw per the take-latest rule.)
- **Anchor shifting:** set an extmark, then `feed` inserts/deletes before, inside,
  and after its range; assert the highlight span moves / clamps / survives
  correctly in the subsequent redraw. This is the core-correctness coverage,
  exercised through real edits.
- **Resync:** set marks, trigger undo/redo or `:edit!`; assert the marks are gone.
- **Namespaces:** marks in ns A are unaffected by `clear_namespace(B)`;
  `create_namespace` is create-or-get by name.
- **Priority:** an extmark over a treesitter-highlighted span wins; a lower-prio
  mark loses. **Id contract:** `set_extmark` returns increasing ids;
  `get_extmarks` round-trips; `del_extmark` returns the existence bool.

## Phases

1. **Core** — `Extmark`, `ExtmarkStore`, namespace registry, anchor shifting in
   `record`, clear in `mark_resync`, the create/set/get/del/clear methods on the
   editor. Synchronous, pure. (Tested through Phase 3's redraw, plus shifting
   asserted once projection exists.)
2. **Lua API** — the five `nvim_*` functions, the `ExtmarkOp` effect enum + drain
   in `effects.rs`, the snapshot mirror + mirrored id allocator, real namespace
   ids.
3. **Redraw projection** — `extmarks_for`, the priority merge into the
   `highlights` payload, `extmark_tick` memoization, client-painter confirmation.
   Black-box tests land here.

## Non-goals (explicitly deferred, kept loud)

- **`nvim_set_decoration_provider` + `on_win`/`on_line`/`on_buf` callbacks** —
  the redraw→Lua per-row seam. Stays `_notimpl`.
- **`vim.treesitter.start` / `highlighter.new`** — the decoration-provider
  consumer. Stays `_notimpl` until the seam above exists or a server-driven
  variant is specced separately.
- **Virtual text / virtual lines / signs / conceal** (`virt_text`, `virt_lines`,
  `sign_text`, `conceal`) — need new client-render primitives (a sign column,
  inline/below-line virtual cells). Each errors at the call site for now.
- **`ephemeral` extmarks** — only meaningful inside a decoration provider's
  `on_line`; deferred with it.
- **Configurable gravity** (`right_gravity`/`end_right_gravity`) — fixed in v1.
- **LSP semantic tokens** — a consumer built *on* this layer, tracked separately.
- **Folding `nvim_set_hl` onto real namespaces** — unblocked here, not done here.
