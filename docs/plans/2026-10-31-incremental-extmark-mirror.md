# An incremental extmark mirror

**The problem.** When any mark in a buffer is set, deleted or cleared, `push_buf_mirror`
(`crates/bemtvi-server/src/effects.rs`, the `structural` branch) re-serializes every
mark in that buffer into the Lua mirror (`btv._extmarks`, which `btv.buf.extmarks` /
`nvim_buf_get_extmarks` read). Each mark costs two rope lookups (`byte_rowcol`), a
clone of its payload strings, a msgpack-to-Lua table conversion, and a rebuild of its
Lua row in `btv._set_extmark_mirror` (`prelude/state.lua`).

So one `set_extmark` costs O(marks in the buffer). A plugin that touches its marks on
every key or scroll pays for every other plugin's marks too: a decor provider
republishing the viewport, a `CursorMoved` handler moving a cursor-word highlight, the
`:s` preview, LSP diagnostics re-anchoring. Measured on 2026-10-31 (debug build, while
reviewing [gutter text](2026-10-31-gutter-text.md)), about 36ms per push with 2 000
marks and about 335ms with 20 000. A `CursorMoved` handler re-setting 30 marks per key
next to 20 000 static marks took 34s for 100 keys, against 0.37s with no static marks.
`main` has the same cost: the [position index](2026-10-31-extmark-position-index.md)
fixed the frame, not the mirror. This breaks the rule that the editor must never freeze.

Text edits are already cheap: they ship positions only, bounded to the edit's window
(`PosScope`, `extmark_positions`), per
[the incremental buffer mirror](2026-08-07-incremental-buffer-mirror.md). This plan
does the same for structural changes.

Surveyed 2026-10-31 at `465c05df` (branch `gutter-text`).

## What already exists

- **A per-namespace counter.** `NsMarks::gen` is bumped by every structural mutation of
  that namespace (`set`, `del`, `clear`) and never by `shift`. The diagnostics
  anchor-trust guard (`lsp/diagnostics.rs`) already relies on it.
- **A per-buffer counter.** `ExtmarkStore::generation`, which the server compares
  against `extmark_gens[buf]` to choose between "positions only" and "everything".
- **The `true` sentinel** in the mirror push, which already means "keep what Lua holds
  for this buffer".

## The design

Narrow the re-serialize from the buffer to the namespaces that changed.

1. **Counters that can't collide.** Today each `gen` counts up from zero inside its
   own slot. Two different slots can therefore carry equal values, either across a
   store swap (`buffer.extmarks = …` in undo) or after `move_namespace_into` brings in
   a snapshot's slot (undo does this for the cursor namespaces). Comparing equal
   would then wrongly skip a push. Stamp both `NsMarks::gen` and
   `ExtmarkStore::generation` from one process-wide monotonic counter (an
   `AtomicU64` in `extmark.rs`: no I/O, so core stays pure and synchronous). Then
   equal stamps mean the same contents. The diagnostics guard only gets stronger:
   a slot that undo resurrected still carries a stamp other than the recorded one.

2. **Positions-only changes say so.** `adopt_positions_from` (undo) moves marks
   wholesale but changes neither membership nor payloads. Today it bumps the store
   generation, which forces the full re-serialize. Give the store a separate
   `positions_reset` stamp that `adopt_positions_from` bumps instead of the
   structural one. The server answers it with `PosScope::All` positions, which is
   O(marks) in integers with no strings or tables. That is acceptable once per
   undo, the same as a line-count-changing edit costs today.

3. **The server tracks namespaces per buffer.** Replace `extmark_gens: HashMap<BufferId,
   u64>` with `HashMap<BufferId, HashMap<u32, u64>>`, the stamps last pushed for each
   namespace. When the store's generation has moved, walk the store's namespaces with
   a new `ExtmarkStore::ns_stamps()` (O(namespaces), skipping the reserved ones as
   today):
   - a namespace whose stamp differs, or that is new, is re-serialized: only its
     marks;
   - a namespace in the old map but not the store (cleared by `clear_all` on reload,
     or emptied and removed) is sent as dropped;
   - every other namespace is left alone.
   Serializing one namespace is the existing per-mark closure, moved into a function
   and applied to `iter_ns(ns)`.

4. **A per-namespace wire shape.** `set_extmark_mirror` takes, per buffer, either
   `true` (unchanged, as today) or a map `ns -> list | false`. A list replaces that
   namespace's rows in `btv._extmarks[bufnr][ns]`, and `false` removes the namespace.
   `ExtmarkMirror` itself is unchanged. A buffer pushed for the first time (no entry
   in the server's map) sends every namespace, which is the same as today's full push.

5. **Lua patches in place.** `btv._set_extmark_mirror` stops rebuilding `marks` from
   scratch. It keeps `btv._extmarks[bufnr]` for every buffer the push names, replaces
   or drops the namespaces the map lists, and drops buffers absent from the push (a
   buffer with no marks, as today). The position pass is unchanged.

The cost of a structural change becomes O(marks in the changed namespaces) +
O(namespaces). A plugin republishing its own namespace pays for its own marks, which
it just did the work to produce anyway.

### Read-after-write

The `set` / `del` / `clear` wrappers in `prelude/api.lua` write through to
`btv._extmarks` within a chunk, and the next push overwrites those rows with the
store's. That stays true per namespace: any namespace a wrapper wrote through to has
had its stamp moved by the queued op, so the push replaces it. One case to test: a
write-through to a namespace whose op then fails in the core, which leaves the stamp
unmoved. Today the full push wipes the stale row; per namespace it would survive.
The fix is for the wrapper to note the namespace as dirty, and for the push to also
re-send dirty namespaces. Alternatively, accept that a failed op raises loud, so the
row can't be relied on. Decide in Phase 1, from a test.

### Remote and web

`push_buf_mirror` and the Lua runtime are the same code in the native, daemon and
wasm edit-host builds (the `native` gate covers transport only). Check
`cargo check -p bemtvi-server --no-default-features` and the browser checks that read
marks (`verify-decor.mjs`, `verify-gutter-text.mjs`).

## As built

- **No `positions_reset` stamp (step 2).** Phase 1 added it and Phase 2 removed it.
  `adopt_positions_from` keeps moving the store generation, which the undo snapshot
  cache compares, and no namespace stamp moves, so the walk re-sends no payload. Its
  one caller, `restore_snapshot`, replaces the text through `mark_resync`, and that
  resync already makes the mirror refresh every position (`PosScope::All`). A
  mutation that ignored the stamp failed no test, because nothing could reach it.
- **Read-after-write.** The one way a queued op failed was the native bridge
  raising on a decoration it can't lower (an unknown `virt_text_pos` / `hl_mode`),
  after the write-through. `btv.buf.set_extmark` now writes its row after queueing.
  Otherwise the core can't fail an op: it clamps positions, and a deleted buffer
  drops out of the push. No dirty-namespace tracking was needed.
- **The remaining cost, and the index per namespace.** After Phase 2 the cost test
  measured ~3.1x a run with no static marks (it was ~75x). Almost all of the rest
  was the core's position index: one sorted vector for the whole store, which any
  structural change dropped and the next frame rebuilt by sorting every mark. Each
  namespace now also keeps its own sorted run, dropped only by a change to that
  namespace (and by undo's position adoption). The combined index is still dropped
  by any structural change, but it is rebuilt by concatenating the runs and merging
  them with the run-adaptive stable sort: close to a copy when one big namespace sits
  beside a few small ones. Edits shift both. The cost test measures ~1.3x and bounds
  it at 2.0. A first version queried each namespace's run per call and merged the
  hits (~1.0x here), but review measured it 2.3x slower than `main` typing beside
  1000 small namespaces with marks on screen, since every frame looped and sorted
  per query. One combined index keeps `main`'s query cost (~2.3x against `main`'s
  ~2.2x on that shape, bounded at 2.8).
- **The sweep (Phase 3, 2026-11-01).** `cargo test --workspace --no-fail-fast` green;
  all 120 example specs pass under `--test-plugin`; the plugin repos dap, diff, help,
  tree, line and markdown-preview pass. colored-pairs (3 provider-paint waits) and
  snippets (`btv.reg.set('+')` now refused for the clipboard) fail identically on
  `main`, so neither is this change. `cargo check -p bemtvi-server
  --no-default-features` builds, and `verify-decor.mjs` / `verify-gutter-text.mjs`
  pass against a fresh `build.sh`.

## Phases

Each phase ends with the suite green and a commit, and pauses for review.

### Phase 1: the failing cost test, and the stamps

- A timing-bounded test in `crates/bemtvi-server/tests/extmark_mirror.rs`, shaped
  like the off-screen cost tests in `extmark_render.rs`: 20 000 static marks in one
  namespace, then a `CursorMoved` handler re-setting 30 marks in another, driven
  through 100 keys. The bound is a small factor of the same run with no static
  marks. Confirm it fails now.
- Correctness tests that must pass before and after. After each of these,
  `btv.buf.extmarks(…, {details = true})` must match the core:
  - mixed set/del/clear across three namespaces;
  - `:e!` reload (`clear_all`);
  - undo and redo with marks added and deleted after the history point;
  - a decor provider's ephemeral namespace across undo;
  - multi-cursor marks across undo (`move_namespace_into`);
  - a namespace cleared to empty.
- The process-wide stamp for `NsMarks::gen` and `ExtmarkStore::generation`, and the
  separate `positions_reset` stamp in `adopt_positions_from`.

### Phase 2: per-namespace push

- `ns_stamps()`, the per-buffer stamp map in the server, the per-namespace
  serializer, and the `ns -> list | false` wire shape in `runtime.rs` and
  `state.lua`.
- The cost test passes and every correctness test still does.
- Mutation check: send every namespace each time. The cost test must fail, and the
  correctness tests must still pass, which shows they don't depend on the narrowing.

### Phase 3: sweep

- `cargo test --workspace`; `--test-plugin` over every example that uses extmarks or
  decor; the plugin repos under `~/work/bemtvi-plugins` that read `btv.buf.extmarks`
  (decor, dap, line).
- Both builds: `--no-default-features`, and the browser checks.
- Update the docstring of `btv._set_extmark_mirror` and the comment on
  `ExtmarkStore::generation`.

## Not in scope

- **Per-mark diffs within a namespace.** A provider republishing 200 viewport marks
  re-sends 200 rows, which is bounded by what it published. Diffing marks would need
  per-mark stamps, and the bounds above don't call for it.
- **The position pass after a line-count change** stays `PosScope::All` (integers
  only), as today.
