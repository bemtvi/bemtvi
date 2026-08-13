# Per-keystroke costs, round 2

`db2b32ae` ("per-keystroke work stops scaling with the buffer") closed four paths
where typing cost grew with everything the buffer held. This plan closes the four
that survived it. They are the *same* bug in different subsystems, and three of
them are literally the same shape the previous plan named and fixed elsewhere:

> per-event work proportional to total state, when only what **changed** — or only
> what is **on screen** — can matter.

## What was measured

The probe is the one `buf_mirror.rs` established: 300 keystrokes into a 5000-line
buffer, debug build, with a `CursorMoved` handler registered so the Rust→Lua mirror
pushes on every key (the normal loaded-config case). **Baseline: 0.49 s.**

| workload | 300 keystrokes | vs baseline |
| --- | --- | --- |
| baseline (nothing loaded) | 0.49 s | 1x |
| 5000-entry quickfix list | 8.70 s | 18x |
| 5000-entry location list | 8.90 s | 18x |
| 5000 diagnostics | 17.5 s | 36x |
| `foldmethod=indent` | 13.0 s | 27x |
| `foldmethod=marker` | 11.5 s | 24x |
| tree-sitter (8000-line Lua file, per 100 keys) | 6.83 s | 20x |
| 143 KB register | 0.52 s | 1.06x |

Measured *not* to be problems, and left alone: the undo-tree mirror (correctly
version-gated — 400 undo blocks cost nothing), LSP `didChange` (genuinely
incremental), the `hlsearch` / `:s`-preview projections (already viewport-scoped),
marks and the jumplist (1.3x for a full jumplist plus 26 marks), and the `bo`
mirror (the previous plan already cleared it).

The window mirror is O(windows) with a fat constant — 13 splits doubled typing
cost — but it is bounded by screen space, not by buffer or list size, so it is a
different (and much smaller) problem. Noted, not fixed here.

## Phase 1 — the two ungated per-tick mirrors

`push_buf_mirror` is the per-keystroke choke point, and two of the mirrors it
pushes rebuild themselves in full on **every** tick with no gate at all.

### Quickfix / location lists

`push_qflist_mirror` (`effects.rs:2749`) says so in its own comment — "Cheap (a
handful of short strings each), so it isn't gated on a dirty flag". True per item;
false in aggregate. Each item costs a fresh Lua table plus 13 field sets, and a
`:vimgrep` across a repo routinely produces thousands. Every window's location
list is torn down and rebuilt alongside it.

This is the same stale-assumption comment the previous plan found at
`extmarks.rs:223` ("the mark set is small and scanned once per frame" — true when
written, false later), and it takes the same fix: the `generation` counter that
gated the extmark mirror.

- `Editor` grows `qf_generation`, bumped wherever a list stack is handed out
  mutably (`qf_cur_mut` / `qf_stack_ensure` — the two `&mut` doors every mutation
  goes through, so a new command cannot forget to bump it).
- The server gates the push on `(generation, the window ids currently holding a
  location list)`. The id list is exact rather than a count, so a window with a
  loclist closing while another opens one cannot alias; it is O(windows) integer
  work to compute, which is what the gate is replacing thousands of table writes
  with.

Bumping on the `&mut` handout rather than at each write site is deliberate: a
caller that takes the reference and writes nothing costs one redundant push, which
is harmless, whereas a write site that forgets to bump is a silently stale mirror.

### Registers

`register_mirror` (`operators.rs:1149`) copies every register's full text into a
fresh `Vec<String>` on every tick, and `set_reg_mirror` copies it again into Lua.
At 143 KB it costs 6% — real but small. It is linear in what is stored, though,
so yanking a large file (`ggyG`) is a cliff, and there is no reason to pay it at
all when nothing has been yanked.

- `Registers` grows a `generation`, bumped in `set` — the single write choke point
  every other method (`record_yank` / `record_delete` / `write_named` /
  `shift_ring` / `set_api`) already funnels through.
- The four read-only specials (`%` `/` `:` `.`) are resolved from live editor
  state, not stored in `Registers`, so the generation alone cannot see them
  change. They are short, so the gate carries them literally: skip the push when
  both the generation and those four strings are unchanged.

## Phase 2 — the diagnostic render projections

`diagnostics_merged` (`lsp/diagnostics.rs:279`) builds a `Vec` over **every**
diagnostic in the buffer, each with an extmark lookup. It is called from the
underline (`:412`), virtual-text (`:494`), sign (`:566`) and statusline-count
(`:384`) surfaces — per window, per frame. Each render surface then builds a
`DiagLineIndex` / `DiagStartIndex`, which buckets every diagnostic through
`byte_to_line`: a rope lookup per diagnostic, three times per frame.

Cost is clean linear in the diagnostic count, and it is overwhelmingly the render
half:

| diagnostics | 300 keystrokes |
| --- | --- |
| 1000 | 3.85 s |
| 2000 | 7.25 s |
| 5000 | 17.5 s |
| 5000, `underline`/`virtual_text`/`signs` all off | 1.44 s |

This is *exactly* Phase 5 of the previous plan, which found `extmark_sign_cells`,
`virt_text_for` and `line_bg_for` each bucketing every mark by
`buf.byte_to_line(m.start)` "when only the ~50 visible rows can contribute". The
fix is the same one, for the same reason: prune to the viewport's byte range with
an integer compare **before** the rope lookup, so only the handful of visible
diagnostics pay for one.

- A viewport-scoped `diagnostics_in_byte_range` feeds the three render surfaces.
  Underline needs intersection (a multi-line diagnostic entering the viewport from
  above must still paint), signs and virtual text key on the anchor line alone —
  the same distinction the extmark prune drew.
- `diag_counts_for` is O(N) by nature (it counts the whole buffer), but severity
  does not depend on position, so it stops resolving spans at all — it counts
  straight off `merged_sources`, skipping the extmark lookup and the
  `TrackedDiagnostic` build entirely.

The failure mode is an off-by-one at either viewport edge silently hiding a
squiggle, so both edges get pinned by tests, exactly as the extmark prune did.

## Phase 3 — computed folds

`refresh_folds` (`editor/fold.rs:655`) runs from the per-keystroke hook
(`editor/mod.rs:2454`) and its cache key includes `changedtick`, so **any** edit
invalidates it and the whole buffer is walked again:

- `compute_indent_folds` reads all N lines (`line_cow` — an allocation each) and
  scans each one's leading whitespace.
- `compute_marker_folds` reads all N lines looking for the marker strings.
- the generic Lua `foldexpr` path (`server/folds.rs::refresh_expr_folds` →
  `eval_foldexpr_lines`) makes **one Lua call per line per keystroke** — measured
  at exactly 1 500 000 calls for 300 keystrokes over 5000 lines.

The observation that makes this tractable: all three sources are per-line
functions of per-line text. An edit changes the text of the rows it touched and no
others, so the *per-line inputs* can be cached and spliced, exactly as the line
mirror splices its rows.

- The core grows a sixth per-buffer edit journal (`fold_edits`), alongside the five
  documented at `buffer.rs:144-172`, with its own drain cursor for the same reason
  they each have one.
- Per buffer, cache the per-line fold inputs — indent levels, marker
  open/close deltas, `foldexpr` values — and on an edit re-derive only the rows the
  journal names, splicing the rest through.
- A `resync` batch, an unfoldable journal, or an option change (`shiftwidth`,
  `foldmarker`, …) drops the cache and recomputes whole. Same fail-safe shape as
  the mirror delta's full-push fallback.

What remains after the splice is `ranges_from_levels` over the cached array: still
O(lines), but pure integer work with no rope reads, no allocation, and no Lua. That
is a deliberate stopping point, and the guard is a ratio, so if it ever stops being
negligible it will show up as a failure rather than as a mystery.

**Tree-sitter folds are not part of this phase.** `compute_treesitter_folds` runs
the `@fold` query over the whole tree per keystroke, which is Phase 4's problem in
a different query; it is measured after Phase 4 lands and dealt with then if it
still matters.

## Phase 4 — the tree-sitter injection layers

`Engine::edit` (`bemtvi-ts/engine.rs:800`) calls `update_injection_layers` on every
edit, which runs the host grammar's injection query over the **entire tree** via
`collect_injection_regions` (`engine.rs:2363`) — a `QueryCursor` with no
`set_byte_range`, unlike `extract_spans` (`engine.rs:2166`), which correctly clips
to the viewport.

Ablating just that rebuild, on real Lua source, 100 keystrokes:

| lines | as shipped | rebuild ablated | no tree-sitter |
| --- | --- | --- | --- |
| 500 | 1.01 s | 0.63 s | 0.14 s |
| 2000 | 2.19 s | 0.77 s | 0.17 s |
| 8000 | **6.83 s** | 1.21 s | 0.32 s |

83% of tree-sitter typing cost at 8000 lines, and it is the part that *scales*:
ablated, cost is near-flat in buffer size (0.63 → 1.21 across a 16x range); shipped
it grows 6.8x. The benchmark file contains **no injected regions at all** — the
buffer pays a full-tree query on every keystroke to rediscover that.

### Considered and rejected: clip the injection query to the viewport

The obvious mirror of `extract_spans`, and wrong here for two reasons the highlight
path does not have:

- **Incremental parse hints stop being valid.** `build_injection_layers` reuses an
  old child tree as the `old_tree` argument of the child's reparse. tree-sitter
  requires that tree to be the previous parse of *this* text with `edit()` applied;
  handing it a tree from an unrelated region produces a wrong parse, not a slow
  one. Today regions keep their identity across edits so this holds; making the
  region set follow the viewport would churn it on every scroll.
- **Combined injections would be cut in half.** An `injection.combined` pattern
  gathers ranges from across the whole buffer into one child tree; clipping the
  query would parse only the visible fragment of it.

### The fix: derive from what changed

tree-sitter already answers "what part of the tree is different" —
`Tree::changed_ranges(old, new)` — which is the primitive neovim's `LanguageTree`
uses to invalidate injections. So:

1. Keep the previous root tree across the reparse and take
   `changed_ranges(old, new)`, unioned with the byte ranges the edits themselves
   touched. The union is not belt-and-braces: `changed_ranges` reports where the
   *syntax* differs, and a same-shape token substitution can leave it empty while
   changing the text a `#eq?` / `#match?` / `#lua-match?` predicate reads.
2. Shift the cached region list through the edits.
3. Drop every cached region intersecting the dirty set, re-run the injection query
   restricted to the dirty ranges, and union the results back in.

A region outside the dirty set is by construction unaffected, so this is coverage-
identical to the full walk. Regions keep their identity, so the incremental parse
hints stay valid — the reason this design was chosen over the viewport one.

### What the measurement corrected  *(done)*

Two things the design above got wrong, both found by measuring rather than reasoning.

**The dirty set has to be derived past the early-outs.** `changed_ranges` walks both
trees, so computing it in `edit()` — before checking whether the language even *has*
an injection query — put an O(tree) cost on every language that injects nothing.
Moving it inside the incremental path took the 8000-line case from 2.04 s to 1.44 s,
against an ablated floor of 1.21 s.

**A region is invalidated by its whole match, not by what it injects.** The first cut
dropped a cached region when the dirty set touched the *content ranges*, which is
wrong for the query markdown actually ships:
`(info_string (language) @injection.language)` reads the fence's language from
outside the content, so rewriting ```` ```rust ```` as ```` ```ruby ```` — same
length, same tree shape — changed the match while touching none of its content, and
the stale rust layer kept painting. Each cached region now carries the byte span of
**every node its match captured**, and invalidation keys on that.

Nothing in the original test set caught it: seven correctness tests and the perf
guard all passed against the broken version. It surfaced only from asking what the
two surviving mutations (dropping the written ranges from the dirty set; never
dropping touched regions) were failing to be caught *by*.

| real Lua, 100 keystrokes | before | after | injection rebuild ablated |
| --- | --- | --- | --- |
| 500 lines | 1.01 s | 0.69 s | 0.63 s |
| 2000 lines | 2.19 s | 0.90 s | 0.77 s |
| 8000 lines | 6.83 s | 1.44 s | 1.21 s |

**A query containing any `injection.combined` pattern falls back to the full
walk.** A combined region-set is accumulated across matches, so a partial re-derive
would produce a partial set. Of the languages that ship queries here only markdown
has one, so the fallback costs almost nothing in practice — and it fails to
*today's* behavior, not to a wrong one.

## Testing

Every phase carries correctness tests first (the project's test-driven bug-fix
rule) and a perf guard second. The guards are **ratios** — the heavy workload
against the light one, timed in the same run — for the reason the previous plan
established: both halves see the same machine load, so they do not flake under a
loaded `cargo test --workspace` the way an absolute bound does.

The previous plan's hardest-won lesson applies throughout and is worth restating,
because two of these phases are gates and gates are exactly where it bites: *a
test that passes whether or not the feature works is worthless.* A mirror gate
that never pushes still passes any test that only reads the mirror once. So each
gate is tested by mutating state through every door that should open it, and by
checking that a door that should **not** open it (a keystroke that changes
nothing) leaves the mirror's Lua table identity intact — the same
table-identity trick `buf_mirror.rs` uses to prove the delta path really ran.

## Follow-up (2026-08-12 audit round): the undo-tree mirror

A later audit round found one more same-shape survivor: `push_undotree_mirror`
re-projects the buffer's **whole undo tree** on every edit of it — O(history)
per keystroke — and pushed even when no plugin ever reads
`vim.fn.undotree`. The first attempt was a reader gate (the
`key_pending_active` pattern): `btv.undotree.get` flips `btv._undotree_register`
on first read, and the server skips the walk + push entirely while it stays
unset. **That gate was reverted**: it breaks the first-read contract in a way
no test change could paper over. The mirror push for a tick happens *before*
that tick's Lua chunks, so a read can only arm the gate for pushes of
*later* ticks — the first read of any session observes a mirror that was
gated off during every edit before it, and returns the empty zero-tree. The
unchanged `undotree_*` tests caught it (mutations, then a read in the first
Lua chunk), but the same staleness would hit any real config that opens the
undo tree *after* editing — `:UndotreeShow` after typing — and there is no
synchronous bridge from a Lua chunk to the editor (bridges only queue ops;
Lua reads mirrors by design). A gate can only be sound when the read itself
is deferred to a later tick, which the synchronous `vim.fn.undotree` contract
forbids. The pre-existing per-buffer `undo_version` fingerprint gate is the
keeper: it bounds re-pushes to buffers whose tree actually changed.

**Still open: the tree itself is never pruned** — bemtvi keeps every undo node
for the session (vim caps this at `undolevels`, default 1000). This is now
the *only* fix for the per-edit walk: wiring `'undolevels'` (options registry
+ apply + BoMirror) and pruning the oldest branch on overflow in `undo.rs`
bounds the walk at the cap. Not done in the audit round: it is an option-
surface feature, not a defect, and the tree walk is inherent to serving a
live undo tree.

## Follow-up (2026-08-12 audit round 3): wire/frame bounds, the extmark index

The third audit round (fresh per-crate scan + adversarial review of the
applied diff) fixed the remaining same-shape defects and recorded the items
that are accepted risk or a different surface entirely:

- **`ExtmarkStore::shift` is now O(log + moved) instead of O(E)** — the
  landed shape is a single lazily-rebuilt position index, not the canonical
  marktree. The first attempt at the index was *too eager* and the benchmark
  caught it: maintaining two per-mark position BTreeSets at the mutation sites
  (`by_start` + `by_end`, 4 tree ops per moved mark in the shift) regressed the
  wide shift to **979 µs/keystroke vs HEAD's 16.5 µs** — a 59x blowup that
  failed `typing_does_not_scale_with_the_diagnostic_count` (4.6x ratio vs the
  3.0 cap), the regression test that originally motivated the index. The
  landed design keeps one index, `sorted: Vec<(u64 start, u64 id, Option<u64>
  end)>`, plus a dirty flag: the five mutation sites (`set_with_gravity`, `del`,
  `clear`, `clear_all`, `move_namespace_into`) just mark it dirty, and the next
  shift rebuilds it in one `sort_unstable` — O(E log E) amortized over the
  mutation batch (the 5000-mark diagnostic set's inserts stay O(1)), instead
  of paid per insert. The shift then splits the vector at
  `partition_point(start < edit start)`: the **covering walk** scans the prefix
  testing `end >= edit start` against the index itself (no mark lookup for the
  check — that's why `end` rides in the tuple; only actual straddlers fetch
  the mark, for its gravity flag), and the **main walk** rewrites each moved
  suffix entry's anchors in place — both gravity maps are monotone
  non-decreasing, so the rewritten suffix keeps its sort and no re-sort is
  needed (verified per piecewise case). Measured after the fix: 139 µs wide
  (edit at byte 0, all 5000 marks move) and 3.3 µs narrow (edit near the end),
  and the regression test passes. The residual per-moved-mark `BTreeMap`
  `get_mut` (~60 ns) is the price of id-keyed lookup — the wide case is 8x
  HEAD, acceptable next to the render cost the test measures — and the
  canonical long-term fix remains **marktree** (a real interval tree over the
  whole store); revisit it when extmark counts per namespace grow past the
  point where the index rebuild on `clear` is the hot path.

  **Revisited in round 4 and reverted.** A fresh adversarial review of the
  landed index proved it *strictly worse than HEAD's flat pass*, not better:
  the covering walk iterates the entire prefix (all E marks when the edit is
  at byte 0 — the wide case the benchmark measures) and the main walk pays a
  `BTreeMap` `get_mut` per moved mark, so the honest bound is O(E) with worse
  constants than the flat `values_mut()` pass it replaced (139 µs wide vs
  HEAD's 16.5 µs; the narrow case is no better at 3.3 µs vs ~13 µs). The
  index never paid off because a position index cannot reduce the shift below
  one visit per mark (a mark's new anchors depend only on its own anchors +
  the edit), and nothing hot consumes a start-sorted index anyway. `shift` is
  back to HEAD's flat O(E) pass. What the round kept from the experiment: a
  per-namespace `gen: u64` generation counter, bumped by structural mutations
  (never by shift), which the diagnostics anchor-trust guard now uses to
  detect an undo-restored extmark store (see `diagnostics.rs` — the old
  count-only guard survived an undo whose restored store held the same number
  of marks, pointing the anchors at the wrong spans). The canonical long-term
  fix remains marktree.

- **The RPC frame path is now bounded at both ends.** The reader already tore
  the connection down past `MAX_FRAME` (64 MiB); the encode side now refuses
  per-method via `encode_checked` (notify/notify_stream: eprintln + drop;
  request: `Err` through the `PendingGuard`; respond: a small error reply) —
  including the Lua-leg guard, where a plugin-built `Value` that encodes past
  the cap fails loudly instead of wedging the wire. The one residual: a plugin
  building an enormous value spends the Lua time before the refusal is
  possible (the size is only known after encoding). `scan_frame`'s depth cap
  was re-aligned with rmpv's actual accounting (scalars charge 1, containers 2,
  bins 2, str/ext 3 — every unit decrements via `checked_sub`) and — the
  subtle part — the budget is **per descending path, not frame-cumulative**:
  rmpv passes `depth` down by value, so a container's children all run on
  `depth - 2` and siblings never accumulate against each other. The first
  attempt charged every value in the frame against one running budget, which
  rejected any redraw wider than ~40 values (a flat frame is cheap for rmpv,
  so the scan and the decoder disagreed and the connection died on the first
  big repaint — the `daemon_lsp` suite caught it). The landed model keeps a
  per-level budget array, so the cap stops pathological *nesting* while
  `MAX_FRAME` is the flood bound and flat frames of any width pass; the
  outbound channel staying unbounded is fine for a queue of ≤64 MiB frames,
  and cancel-growth is already bounded by the `PendingGuard` (`notify_stream`
  has a bounded backpressured channel).

- **A silent LSP server keeps its pending entries — accepted risk, both
  legs.** Neither the native manager nor the wasm `SyncLspClient` has a
  per-language-request timeout, and this round confirmed that is the right
  shape: a wedged handshake is bounded (`INIT_GRACE` 30 s kill on the native
  leg, the 4096-entry `QUEUED_CAP` on the pre-handshake queue on the wasm
  leg), while *post*-handshake silence on a language request leaves at most
  the editor's in-flight requests pending (one token per request), and a
  timeout would fabricate a degraded reply for a merely slow server mid-
  request. The native leg's respawn/backoff breaker handles a truly dead
  server at the server level. The wasm leg's string-id fall-through (a
  response echoing an id we never sent) was the one silent path in that
  accounting and now logs loud instead.
