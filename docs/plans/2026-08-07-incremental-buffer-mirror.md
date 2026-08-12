# Incremental buffer mirror

## The problem

`push_buf_mirror` (`crates/bemtvi-server/src/effects.rs:1893`) re-serializes a
buffer's **entire** line array whenever its `changedtick` moves:

```rust
let lines = if fresh {
    Some(self.editor.lines_of(id).unwrap_or_default())   // every line, every edit
} else {
    None
};
```

and `btv._set_buf_mirror` (`crates/bemtvi-lua/src/prelude/state.lua:1827`) replaces
`btv._bufs` wholesale, carrying the previous `lines` over only for buffers whose
tick did *not* move.

So one keystroke in insert mode on a 50k-line buffer costs 50k Rust `String`
allocations (`lines_of`) plus 50k Lua string allocations and a fresh 50k-element
Lua table — per keystroke. Typing is O(lines x keystrokes), and the GC pressure
scales with file size. This is the "per-event work must not scale with total
buffer size" rule in CLAUDE.md, violated on the single hottest path.

It is gated — the mirror is only pushed when Lua can observe it — but any config
with an autocmd handler, a decor provider, or a statusline expression hits it,
which is the normal loaded-plugin case.

## Considered and rejected: a pull-based getter

The obviously-cheapest design is to stop mirroring lines at all and have
`btv.buf.lines(b, s, e)` call a Rust function that reads the rope for just the
requested range — O(requested), not O(buffer).

Rejected: it breaks the architecture's load-bearing invariant that **Lua never
touches the live rope or `Editor`** (the mirror exists precisely so synchronous
getters never reach the editor). The mirror is pushed at controlled points so a
Lua chunk sees one *consistent* snapshot for its whole duration; a getter reading
live state mid-chunk could observe a half-applied batch, and it would require
threading the editor into every install closure. Keep the push model; make the
push incremental.

## Design: a fifth edit journal

The core already records exactly what is needed. `BufferEdit`
(`crates/bemtvi-core/src/buffer.rs:26`) carries `start_point` / `old_end_point` /
`new_end_point` as `(row, byte-col)`, and `EditBatch` pairs the edits with a
`resync` flag set when the whole rope was replaced.

There are already **four** independent journals — syntax, LSP, Lua-treesitter,
jumplist — each with its own drain cursor, documented at `buffer.rs:144-172` as
deliberate: one shared journal would let whichever consumer drains first starve
the others. The mirror is a fifth consumer with its own drain point, so it gets a
fifth journal (`mirror_edits` / `mirror_resync`). Reusing the Lua-treesitter
journal is **not** an option — `push_buf_mirror` already drains it for `on_bytes`
(`effects.rs:2065`).

### Per changed buffer, the push sends one of

- **Full** `lines: Vec<String>` — as today. Used when the buffer was not in the
  previous mirror (first appearance), when the batch is `resync` (undo/redo,
  `:e`, reload — the rope was replaced and deltas are meaningless), or when the
  batch cannot be safely coalesced (below).
- **Delta** `{ start, old_end, lines }` — replace mirror rows `[start, old_end)`
  with `lines`. Cost is O(edited rows), not O(buffer).

### Coalescing a multi-edit batch

Content can only be read from the **final** rope (the journal is drained after
every edit has landed), so the batch must collapse to a single line span whose
replacement text is read from the final rope. Each edit's rows are expressed in
the document as it stood *before that edit*, so folding them requires mapping
through the shifts of the preceding edits.

The fold is done only where it is trivially sound, and falls back to a full push
otherwise:

- One edit: precise span, no mapping needed.
- Several edits that move strictly forward and do not overlap (each edit's
  `start_point.row >= the previous edit's new_end_point.row`) — the common batch
  shape: a multi-key insert, a `:s` walking down lines. Fold by accumulating the
  running row shift.
- Anything else (out-of-order or overlapping edits): **full push**. Fails safe to
  exactly today's behavior.

If the guard test in Phase 2 shows the fallback firing on real workloads, widen
the fold then — not speculatively.

### Two hazards, both already closed

- **Double-apply.** Splicing is not idempotent the way a full replace is, so a
  Lua-side write-through would corrupt the array. There is none:
  `btv.buf.set_lines` / `set_text` validate synchronously and only *queue* a
  `BufOp` (`crates/bemtvi-lua/src/install.rs:2337`), landing through the core,
  which journals it like any other edit. (The doc comment on
  `LuaRuntime::set_buf_mirror` claiming "`set_lines` write-through mutates this
  same mirror" is stale and gets fixed.)
- **Aliasing.** In-place splicing would be visible to anyone holding the array.
  `btv.buf.lines` copies into a fresh table (`api.lua:418`), so no caller holds it.

## Phases

### Phase 1 — the incremental path, end to end  *(done)*

- `crates/bemtvi-core/src/buffer.rs`: `mirror_edits` / `mirror_resync` fields,
  written in `record()` and cleared/flagged in `mark_resync()`, drained by
  `take_mirror_edits()`.
- `crates/bemtvi-core/src/editor/buffers.rs`: `take_mirror_edits_of(id)`,
  alongside `take_lua_ts_edits_of`.
- `crates/bemtvi-lua/src/runtime.rs`: `BufMirror.lines` gains a delta form; the
  serializer carries `{ start, old_end, lines }`.
- `crates/bemtvi-server/src/effects.rs`: drain the journal in `push_buf_mirror`,
  coalesce, and emit full-or-delta.
- `crates/bemtvi-lua/src/prelude/state.lua`: `btv._set_buf_mirror` splices a delta
  into the retained array; full and absent behave as today.
- Correctness tests: the mirror's contents after a delta must be identical to a
  full push — single-line edit, multi-line insert, multi-line delete, join,
  whole-buffer replace, undo/redo, `:e` reload, a first-seen buffer, a queued
  `btv.buf.set_lines` round-trip, and `on_lines`/`on_bytes` still firing right.

### Phase 2 — remaining

The regression guard landed with Phase 1 rather than after it, since shipping the
fix unguarded made no sense:
`typing_in_a_large_buffer_does_not_scale_with_the_buffer` in
`crates/bemtvi-server/tests/buf_mirror.rs` feeds 300 keystrokes into a 20k-line
buffer with an autocmd handler registered (so the mirror pushes every key).

Measured on that exact run, debug build:

| | 300 keystrokes / 20k lines |
| --- | --- |
| whole-buffer push (before) | 47.6 s |
| line delta (after) | 0.79 s |

about 60x, and the bound is 15 s — roughly 3x margin against the old behavior
returning, 19x headroom over the current one.

**Measured** (same harness, debug build, 300 keystrokes), with the extmarks
asserted to exist so the bench can't pass by creating none:

| | 300 keystrokes |
| --- | --- |
| 0 extmarks / 5k lines | 0.50 s |
| 5000 extmarks / 5k lines | **29.2 s** |
| 1 buffer open | 0.46 s |
| 61 buffers open | 0.47 s |

- **`bo` mirror: no change.** 61 buffers cost ~2% over one. It is O(#buffers) with a
  small constant and never approaches the line mirror's problem. Leave it alone.
- **Extmark mirror: the same bug, 59x.** ~97 ms per keystroke, and it matters more in
  practice than the line mirror did — any diagnostics / git-sign / rainbow-paren /
  semantic-token plugin puts thousands of marks on a buffer.

## Phase 3 — the extmark mirror

Every push rebuilds `Vec<ExtmarkMirror>` for **every** buffer holding marks, whether
or not that buffer changed, and each mark costs two rope lookups (`byte_rowcol`), up
to seven `String` clones, a Lua table, and ~14 field sets.

The asymmetry to exploit: **an edit changes only `row`/`col`/`end_row`/`end_col`.**
`hl_group`, `priority`, `sign_text`, `sign_hl_group`, `line_fill_*`, `line_hl_group`
and the gravity flags are fixed for a mark's lifetime — `ExtmarkStore::shift` touches
nothing but the byte anchors. So split the push:

- **Structural push** (the full `ExtmarkMirror`, as today) — gated on a per-store
  `generation` bumped by `set` / `clear` / `clear_all` but *not* by `shift`. A buffer
  whose mark set didn't change never re-serializes its decorations.
- **Position push** — for a buffer whose text changed, a flat
  `[ns, id, row, col, end_row, end_col]` integer array (`-1` for an absent end)
  applied in place by Lua. One table for the whole buffer, no per-mark allocation,
  no strings.

Both are additionally gated per buffer: an untouched buffer's marks can't have moved,
so it contributes nothing — which alone fixes the multi-decorated-buffer session that
today pays for every buffer on every keystroke.

### What the measurement corrected  *(done)*

The design above was written against the wrong cost model, and the split alone bought
only ~15% (29.2 s → 24.9 s). Instrumenting rather than guessing again:

```
total 25.0s │ push_buf_mirror 22.0s │ extmark_build 19.7s │ extmark_lua 2.2s │ redraw 2.6s
```

The dominant cost was never the serialization — it was `byte_rowcol`'s **two rope
lookups per mark per push**, converting each byte anchor back to `(row, col)`. So the
structural gate is necessary but not sufficient; what actually matters is a third
idea the plan had only half-stated:

> **Sliding a byte anchor is not the same as changing a row/column.** Typing a
> character on line 5 moves the anchor of every later mark, but their `(row, col)` is
> *identical* — only marks on the edited line move.

So the position push is scoped to a byte window (`PosScope`): `lo` at the edited
line's start, and `hi` closing at the next line's start when the edit left the line
count unchanged (open otherwise, since a line-count change shifts every later row). A
mark is in scope if *either* edge falls in the window — a range starting on an earlier
line but ending inside the edited one still needs refreshing.

**Only a single-edit batch is bounded.** The first cut derived the window across a
whole batch and was wrong twice over; it crashed `lsp_features` (a formatter applying
several edits in one tick) and was caught only by the full suite:

- Each edit's byte offsets are expressed against the buffer as it stood before *that*
  edit. An earlier edit's `new_end_byte` can therefore point past the final rope, and
  resolving it panics the server outright (`index is out of bounds` in `line_start`).
- Quieter, and worse: a batch whose per-edit row changes **cancel to zero** still moved
  every mark between the two edits. Bounding on the net delta reports them unmoved and
  the mirror goes silently stale.

Folding a batch soundly needs the same forward/non-overlapping mapping
`fold_mirror_edits` does. Since the typing path — the one that matters — produces a
single edit per tick, multi-edit batches simply refresh all of that buffer's marks.
The measured win is unaffected. If multi-edit batches ever show up hot, do the fold
properly rather than widening the window heuristically.

The lesson for the test suite: every original extmark test drove edits with one `feed`
per tick, so none of them ever built a multi-edit batch. Queued `btv.buf.set_lines` /
`set_text` calls in a single Lua chunk do, and that is what
`a_row_preserving_batch_that_shrinks_the_buffer_does_not_crash` uses.

| 300 keystrokes, 5000 marks / 5k lines | |
| --- | --- |
| before | 29.2 s |
| structural gate only | 24.9 s |
| + dirty window | **3.05 s** |
| mark-free baseline | 0.52 s |

Ablating the position pass entirely gives 2.95 s, so the mirror's own remaining
contribution is ~0.1 s: **essentially eliminated** (from ~19.7 s). The ~2.4 s still
separating this from the mark-free baseline is core `ExtmarkStore::shift` (which walks
every mark on every edit) plus the redraw decoration projection — a different
subsystem, deliberately left alone here.

## Phase 4 — the redraw projection

The two follow-ups above were measured before any work, and the measurement
**overturned their stated priority**. Probing each suspect over the same 300
keystrokes / 5000 marks run:

| | of the ~2.5 s over a mark-free buffer |
| --- | --- |
| redraw projections (`bemtvi-server/src/extmarks.rs`) | **1927 ms (77%)** |
| `Buffer::virt_lines_by_line` | 350 ms (14%) |
| `ExtmarkStore::shift` | **51 ms (2%)** |
| undo-snapshot `extmarks.clone()` | 0 ms |

- **`ExtmarkStore::shift` is not worth touching.** It was listed first as the
  suspect — the O(marks)-per-edit scan looked damning — and it accounts for 2%. The
  anchor-ordered index sketched for it would have been a large change to the core
  store for nothing. (Third time this file has recorded a wrong guess corrected by a
  probe; the pattern is consistent enough to just measure first.)
- **The cost is the redraw**, and specifically its shape: `extmark_intervals` takes a
  **single line** (`line_idx`, `line_start`, `line_len`) and scans every mark in the
  buffer to clip it. Called once per visible row, a 50-row viewport with 5000 marks
  does ~250 000 mark visits per window per frame. `virt_text_for`,
  `extmark_sign_cells`, `line_bg_for` and `virt_lines_by_line` each scan once per
  frame on top of that. A comment at `extmarks.rs:223` still asserts "the mark set is
  small and scanned once per frame" — true when it was written, false now.

Two fixes, both measured:

- **`HlMarkIndex`, built once per frame.** `extmark_intervals` now queries a sorted
  index instead of re-scanning. Bucketing by *line* was the obvious shape and is the
  wrong one — deriving each mark's line needs `byte_to_line`, which is the very rope
  lookup that made the mirror slow, so the index sorts by **byte** anchor and answers
  a line's `[lo, hi)` range directly. Overlap needs care: sorted-by-start alone loses
  a long mark hiding behind a short one, so the index carries a running max of every
  earlier `end` and prunes on that. Only marks that can paint (a `hl_group` *and* a
  range) are kept, each with its original enumerate index so the source-layering
  order is byte-identical to the old scan.
- **An O(1) gate on `virt_lines_by_line`.** It is called from the view, cursor and
  mouse walks, and filtered every mark each time to find virtual lines that usually
  do not exist. `ExtmarkStore` now counts marks carrying `virt_lines` (maintained on
  set / replace / del / clear / clear_all / namespace move — `shift` never changes
  it), so a buffer with none returns empty immediately.

| 300 keystrokes, 5000 marks / 5k lines | total | render | virt_lines | shift |
| --- | --- | --- | --- | --- |
| entering Phase 4 | 2.98 s | 1927 ms | 350 ms | 51 ms |
| + `HlMarkIndex` | 1.42 s | 202 ms | 349 ms | 51 ms |
| + virt-lines gate | **1.07 s** | 200 ms | 0 ms | 51 ms |
| mark-free baseline | 0.47 s | — | — | — |

A mark-heavy buffer is now within ~2.3x of a mark-free one, from ~57x when this
plan started (29.2 s).

### Phase 5 — the other payload shapes

Measuring what was left turned up something the earlier benchmarks had been hiding:
**every measurement so far used `hl_group`-only marks, which is the best case.** A
highlight mark never enters `extmark_sign_cells`, `virt_text_for` or `line_bg_for`,
so those three paths had never been exercised at all. Re-running per payload shape
(300 keystrokes, 5000 marks, 5000 lines; probe deltas, since the counters accumulate
across a single-process run):

| marks carry | total | hot function | its cost |
| --- | --- | --- | --- |
| nothing | 0.47 s | — | — |
| `hl_group` only | 1.06 s | `extmark_intervals` | ~0 ms |
| `sign_text` | **6.50 s** | `extmark_sign_cells` | 5651 ms |
| `virt_text` | **6.53 s** | `virt_text_for` | 5672 ms |
| `line_hl_group` | **6.26 s** | `line_bg_for` | 5446 ms |

So the shapes real plugins actually use — git signs, inlay hints, diagnostic line
highlights — cost ~13.8x a mark-free buffer, not the ~2.2x the highlight benchmark
reported. `shift` (~45 ms) and the `HlMarkIndex` build (~55 ms) were confirmed
negligible for a third time.

The cause is the same one twice over: each of the three buckets marks by
`buf.byte_to_line(m.start)` — a rope lookup — **for every mark in the buffer**, when
only the ~50 visible rows can contribute. All three key marks by their *anchor* line
only (no range spanning), so pruning to the viewport's byte range before the lookup
is exactly equivalent: an integer compare for every mark, a rope lookup for the few
that are visible.

| marks carry | before | after |
| --- | --- | --- |
| `sign_text` | 6.50 s | **0.96 s** |
| `virt_text` | 6.53 s | **1.02 s** |
| `line_hl_group` | 6.26 s | **0.95 s** |

Every payload shape now sits at ~2x the mark-free baseline. The `hl_group` case is
unchanged at 1.06 s (it pays the index build the others skip).

The prune's failure mode is an off-by-one at either viewport edge silently hiding a
decoration, so both edges are pinned by tests, and a second **ratio guard** uses a
`sign_text` mark — deliberately a different payload from the highlight guard, because
that is precisely the blind spot that let this sit unmeasured. Removing the prune
puts it at 13.6x against a 3.2 threshold.

What remains over baseline is the position mirror, ~45 ms of `shift`, and the index
build. Nothing individually large enough to justify more surgery — and on this plan's
record (four wrong guesses, each corrected by a probe), not without measuring first.

The perf guard here is a **ratio** (mark-heavy vs mark-free typing, both timed in the
same run) rather than a wall-clock bound. It measured 2.2-2.3x across repeated runs
and reverting either fix pushes it past 4x, so the 3.2 threshold keeps ~40% headroom
while still failing on a regression — and because both halves see the same machine
load, it does not flake the way an absolute threshold does under a loaded
`cargo test --workspace`.

The counter is the risky part of this: drift it *low* and virtual lines silently stop
rendering. `extmark_render.rs` exercises every removal path through what actually
renders rather than through the private count, and mutation testing drove the test
set — the first version passed with `del` decrementing unconditionally *and* with the
max-end prune replaced by a per-mark check, i.e. it caught neither real bug.
