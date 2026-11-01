# A position index for extmarks

**The problem.** Every per-frame extmark layer finds its marks by walking every mark
in the buffer: the highlight index, virtual text, line backgrounds, signs, gutter text
and line fills each call `ExtmarkStore::iter_all` and keep the ones anchored on screen,
and line fills don't prune to the viewport at all. Diagnostics walk every diagnostic
anchor the same way, and the Lua mirror walks every mark after each edit to find the
ones it moved. So a frame costs O(marks in the buffer), not O(marks on screen): 20 000
marks of any kind cost ~3.5x a bare buffer per keystroke, measured while
[gutter text](2026-10-31-gutter-text.md) was built. A profile of that run spends a
third of its samples iterating the store's B-trees.

Surveyed 2026-10-31 at `b8d8e79d`.

## Why there is no index today

`ExtmarkStore` keys each namespace's marks by id. A comment on `shift` explains why: an
edit must visit every mark anyway (an anchor's new position depends on itself and the
edit), and an earlier attempt at keeping a position index current per mark measured
59x slower. That attempt re-keyed a map entry per mark per edit.

## The design

A sorted vector of `(start, end, ns, id, gravity)` entries, ordered by
`(start, ns, id)`, held lazily on the store.

- **Built on demand.** The first query after a structural change (a mark set,
  deleted or cleared, a namespace moved, an undo adopting positions) collects and sorts
  the entries: O(E log E), once.
- **Kept sorted across edits, cheaply.** `shift` already visits every mark. When an
  index exists it also updates the vector in one flat pass with the same gravity rules.
  An edit replacing `[s, old_end)` with text ending at `new_end` leaves entries before
  `s` in place and moves those at or past `old_end` by one delta, so their order
  holds. Only entries inside the replaced region can reorder, and they all land on `s`
  or `new_end`; that run is re-sorted. Typing on a line re-sorts the marks on that
  line, not the buffer.
- **Range queries.** `starting_in(lo, hi)` is two binary searches. `reaching(lo, hi)`,
  for ranges that start above the viewport and end inside it, walks a max-end segment
  tree over the vector: O((k + 1) log E) for k results, so one long mark doesn't make
  every query walk the marks before it. The tree is rebuilt lazily after an edit.
- **Not cloned.** Undo snapshots clone the store; a clone starts without an index and
  builds one if it's ever queried.

The vector duplicates positions the marks hold. Both are updated by the same `shift`,
so they can't drift, and every structural change drops the index.

## What uses it

- `HlMarkIndex::build` queries `reaching` instead of filtering every mark. Its
  layering order was the store's iteration order, which across namespaces followed a
  `HashMap`'s; it becomes `(ns, id)`, the same as before within a namespace and now
  deterministic across them.
- Virtual text, line backgrounds, signs, gutter text: `starting_in` over the
  viewport.
- Line fills: `starting_in` over the viewport, where they used to walk every mark.
- The diagnostic render surfaces: `reaching`, filtered to the diagnostic namespace.
- The Lua mirror's position refresh after an edit: `reaching` over the edit's window.

Not changed: `Buffer::virt_lines_by_line` builds a map of every virtual line in the
buffer for the scroll and cursor arithmetic. It is O(virtual-line marks), gated to
nothing when there are none, and other kinds of marks never reach it.

## Done

One finding the plan didn't predict. With every layer on the index, a frame with
20 000 off-screen marks still cost ~1.9x a bare buffer, the same for every kind of
mark. The cost wasn't in a layer: `Editor::cursor_marks`, which
`has_secondary_cursors` calls on every key, filtered every mark in the buffer for the
multi-cursor namespace, so with no secondary cursors it visited all of them to find
none. It reads that namespace's own map now. With both fixes the off-screen cost is
~1.15x a bare buffer on average across the seven kinds (worst 1.3x, virtual text).

The re-sort in `Positions::shift` is guarded by a `debug_assert` that the entries stay
sorted, which the test suite (a debug build) trips: the reordering test passes without
the re-sort if the assertion is also removed, because the binary search happens to land
right on that layout.

## Tests

Black-box through the server, in `tests/extmark_render.rs` beside the existing
viewport-pruning tests: a frame's cost with 20 000 off-screen marks of each layer's kind
stays within a small factor of a bare buffer while moving the cursor; marks with mixed
gravity collapsed by a deletion still render on the right rows; a range mark starting
far above the viewport still paints; and the existing pruning, overlap and nesting tests
keep passing.
