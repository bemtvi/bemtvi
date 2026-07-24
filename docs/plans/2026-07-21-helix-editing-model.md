# Helix editing model — selection-first grammar on a shared selection engine

> **Status: shipped** (all five phases; opt-in via `:helix` / `nx.helix.enable()`).
> This doc is the design record: the phase plan it grew from, corrected to the
> as-built shape, with the deviations from the original plan and the remaining
> gaps called out at the end. The code lives in
> `crates/nxvim-core/src/editor/helix.rs` (grammar + verbs),
> `crates/nxvim-core/src/editor/selection.rs` (the shared range vocabulary), and
> `crates/nxvim-lua/src/prelude/helix.lua` (the bundled keymap plugin).

## What this is (and is not)

This adds Helix's **editing model** — selection-first (noun→verb), multi-selection
as the always-on default — not merely Helix keybindings. Rebinding keys is the
easy 20%; the hard 80% is that a *motion means something different* in Helix.

- **vim / nxvim default:** verb→noun. The cursor is a **point** (`Cursor { line,
  col }`). `d` sets a pending operator and *waits* for a motion. Selection exists
  only transiently in visual mode (`Editor::cursor` + `Editor::visual_anchor`,
  alive only while `mode.is_visual()`).
- **Helix:** noun→verb. Every cursor **is a range** (`anchor..head`); a bare
  cursor is a width-1 selection. A selection is **always present** and persists
  across every keystroke. `w` *moves and re-selects* the next word on **every**
  selection; `d` then acts on the current selections immediately, never waiting.

So this cannot be expressed by remapping keys to key-strings: the semantics of the
motions themselves change, and there is no "operator-pending" wait state. That is
why the model is native Rust (a genuine core-grammar constraint, per the design
principles) while the *key layout* ships as a bundled `nx` plugin.

## Design decision (settled, held up)

**Separate mode, shared engine.** `Mode::HelixNormal` / `Mode::HelixSelect` with
their *own* parse step (`Editor::handle_helix`), sharing the selection/operator
*engine* with the vim grammar rather than threading Helix input through vim's
operator-pending `PendingCommand` state machine — the two grammars disagree about
what a motion does. Vim behavior is untouched; Helix is opt-in per session.

Rejected: a full grammar fork (duplicates the multicursor machinery, two grammars
drift) and a global vim|helix build switch (harder commitment, no mixing).

## The shared selection engine (Phase 1 — as built)

The original plan called for making a `Selections` set the *source of truth*,
with `Editor::cursor` demoted to a cache. **As built it is the inverse — a
projection seam, not a store swap** — because the existing stores already have
the right lifecycle and thousands of read sites:

- The **vocabulary** is `Range { anchor: Cursor, head: Cursor }` +
  `Selections { ranges, primary }` (`editor/selection.rs`). `head` is the moving
  end (where the block cursor draws); `anchor` is the end extend-mode leaves put;
  both ends are inclusive; a bare cursor is a point range (`anchor == head`) —
  Helix's width-1 minimum.
- The **stores** are unchanged: the primary lives in `Editor::cursor` +
  `Editor::visual_anchor`; each secondary is a `CURSOR_NS` (head) + paired
  `ANCHOR_NS` (anchor) extmark, so secondaries auto-shift through the buffer's
  edit choke point for free.
- The **seam** is `Editor::selections()` / `Editor::set_selections()`
  (`editor/multicursor.rs`): project the stores into one `Selections` (primary
  first, secondaries by head byte), transform it, write it back. Whether the
  primary carries a distinct anchor is decided by `Mode::shows_selection()`
  (visual **or** Helix) — the one predicate that lets Helix reuse the visual
  selection's representation, rendering, and multi-cursor sweep wholesale.
  `for_each_cursor` keys its per-cursor anchor pairing on the same predicate.

Existing machinery promoted, not rebuilt: `for_each_cursor` / `edit_each_cursor`
(fan-out apply, one undo group), `apply_operator_to_range` / `visual_range_lw`
(range operators), `secondary_selections` (rendering), `visual_swap_ends`
(itself re-expressed as a flip through the seam).

## The grammar (Phase 2 — as built)

`Editor::handle_helix` owns Helix input; its transient state (`helix_count`,
`helix_find`) is deliberately **outside** vim's `PendingCommand`. The hardwired
alphabet (usable and testable with no plugin): counts, `h/j/k/l` + arrows,
`w`/`b`/`e`, `f`/`t`/`F`/`T` (a find-target stage), `0`/`^`/`$`/`G`, `v` (toggle
select), `:` (ex line; resumes Helix on close so `:helix` can toggle out), and
`<Esc>` (select→normal; in normal: collapse to a point, drop secondaries).

- **Move-and-select vs. extend.** In `HelixNormal`, word/find motions *select*
  (anchor ← old head, head ← landing); plain char/line motions collapse to a
  point at the target. In `HelixSelect` every motion moves only the head.
- Non-word motions resolve through the shared `resolve_motion`/`apply_movement`
  engine (curswant, folds, EOL stickiness match vim), applied per-range.
- **Word motions are Helix-semantic, hand-rolled** (`helix_word_step`): they
  re-select a whole word region (both ends), scanning one line at a time so a
  selection never spans a line break. `w` selects the word *plus trailing
  whitespace*, stopping just before the next word (vim's `w` lands on it), and
  always advances — even across adjacent runs like `on.`; `e` lands on the next
  word end, folding in leading whitespace when starting from a word end; `b`
  lands on the previous word start. A line's leading indentation counts as its
  own word (Helix's rule). At end/start of line the motion jumps to the nearest
  non-empty line and selects a fresh word there — blank lines are skipped and
  the newline is never selected.
- `W`/`B`/`E` are true WORD (long-word) motions: the same scanner with the
  word/punct classes collapsed (a run of any non-blank chars is one WORD),
  consuming the `big` flag vim's grammar threads through `Motion::Word(big)`.

## Immediate-apply verbs (Phase 3 — as built)

`d` `c` `y` `>` `<` `=` `~` act on the current selections **now**
(`helix_operate` / `helix_operate_multi` in `editor/operators.rs`), reusing
`apply_operator_to_range` + `edit_each_cursor` (one undo group across all
selections). `d`/`>`/`<`/`=` collapse each selection to a point; `y` and `~`
*keep* the selection (Helix leaves a yank highlighted); `c` deletes and opens a
(multi-cursor) Insert whose `<Esc>` resumes `HelixNormal` via
`Editor::base_normal_mode`. Register-writing verbs abort whole on a read-only
register / unavailable clipboard, leaving the selection intact.

**Paste semantics (decided here):** `p`/`P` paste after/before the *selection*
(not the cursor char): each selection collapses to its high/low end and the
shared vim paste runs there. With multiple selections each pastes **its own**
slice of the last multi-yank (`paste_multi`, paired in document order), falling
back to the unnamed register when the counts don't line up.

## Selection verbs with no vim analog (Phase 4 — as built)

- `x` — extend line-wise; when already covering full lines, grow `count` lines
  downward (Helix's `extend_line_below`).
- `%` — select the whole file (dropping secondaries).
- `_` — trim each selection to its non-whitespace content.
- `;` / `,` — collapse each selection to its head / keep only the primary.
- `Alt-;` — flip anchor and head.
- `(` / `)` — rotate which selection is primary through document order.
- `C` / `Alt-C` — copy the primary selection onto the next/previous line(s),
  each copy becoming the new primary (multi-selection growth without a regex).
- `s` / `S` / `K` / `Alt-K` — the regex transforms: select-within / split-on /
  keep-matching / remove-matching. Each opens a `/`-style prompt
  (`CmdlineKind::HelixRegex`) sharing the search history; `<CR>` applies via
  `helix_apply_regex`, `<Esc>` cancels. Matching is per-line (like `/`), only
  matches wholly inside a selection count, and a transform that would leave no
  selection is refused. **Bonus not in the plan:** while the `s`-family prompt is
  open, the would-be selections preview live through the `incsearch` highlight
  channel, clipped to the captured selection ranges (`helix_regex_ranges`) — no
  client changes needed.

Note `Alt-C`/`Alt-K` are spelled `<A-S-c>`/`<A-S-k>` in tests/maps: modified keys
carry shift in the `S-` flag, not the letter case (neovim's key-casing model).

## Named-action registry + the plugin (Phase 5 — as built)

`Editor::apply_helix_action(name, count)` exposes **every** verb by name —
`extend_line_below`, `select_regex`, `flip_selections`, … mirroring Helix's own
command names so a Helix user's config muscle-memory carries over. It is the
single dispatch: the hardwired keys in `handle_helix` route through it too, so a
key and its name can never drift. Unknown names **fail loud** (`E5108` at the
server). `enable_helix`/`disable_helix` work from any mode; everything else
requires an active Helix mode. The count falls back to the digits typed before
the verb (`helix_count`) when the caller passes none.

The seam to Lua is `nx._helix_action(name, count?)` (queued, drained by the
server into `apply_helix_action`). The bundled opt-in plugin
(`prelude/helix.lua`) publishes `nx.helix.enable/disable` and
`nx.helix.actions.<name>`, and binds only what a bare key can't reach: insert
entry (`i`/`a`/`I`/`A`/`o`/`O` — collapse to the selection edge, then a
multi-cursor Insert), the goto `g` menu (`gg`/`ge`/`gh`/`gl`/`gs` + LSP
`gd`/`gy`/`gr`/`gi`), the `<Space>` leader menu (pickers + LSP), and `u`/`U`
undo/redo. All maps are `default = true` so user maps win. `examples/helix/`
is the runnable walkthrough.

### Mode/keymap wiring (the facts that matter for integration)

- Mode codes are `hn`/`hs` (`Mode::short_code`), statusline `HELIX`/`HELIX-SEL`
  — distinct from `n`/`v` so `ModeChanged` fires and plugins can tell them apart.
- Both Helix modes share one keymap bucket, `'h'` (`nx.keymap.set("helix", …)`),
  which **falls through** to the native `handle_helix` grammar on no match (like
  the multicursor `'m'` bucket) — Helix stays usable without the plugin.
- The keymap disambiguation **oracle is off** in Helix modes: it folds over the
  *vim* grammar (`command_status`), which does not describe Helix, so a mapped
  Helix prefix must never be released early by it.
- `Editor::helix` (the session flag) outlives mode switches: Insert opened by a
  Helix verb returns to `HelixNormal` on `<Esc>` (`base_normal_mode`), and a
  `:`-command line resumes the Helix mode it was opened from.
- Mouse maps gate `HelixNormal` on the `n` flag and `HelixSelect` on `v`.
- The multicursor placement mode (`<A-c>`) remains a vim-side affordance; it
  shares the same extmark set, and Helix's `C`/regex spawners are the Helix-side
  way to grow a multi-selection.

## Testing (black-box, per CLAUDE.md)

Six suites behind `tests/editing.rs`: `helix_motions` (mode entry, word-motion
semantics incl. the regression cases, extend, counts, find), `helix_verbs`
(immediate-apply + collapse/keep/Insert-resume), `helix_selections`
(`x`/`%`/`_`/`Alt-;`/`;`), `helix_multi` (`C`/`Alt-C`/rotate + multi-verb
sweeps), `helix_regex` (`s`/`S`/`K`/`Alt-K`, live preview, invalid-pattern
E383, `p`/`P` incl. per-selection paste), `helix_actions` (registry, fail-loud,
plugin maps, bucket fall-through, enable/disable). Assertions ride the rendered
selection spans (`view_selection`), buffer lines, cursor, and `mode()` — the
same way the multicursor suite asserts. Each new verb was mutation-tested.

## Deviations from the original plan

- **Phase 1 inverted:** a projection seam over the existing stores
  (`selections()`/`set_selections()`), not a `Selections`-as-source-of-truth
  refactor. Same guarantee (vim suites unchanged), far less churn; the extmark
  store keeps secondaries edit-stable for free.
- **Range ends are `Cursor` (line, col), not byte offsets**, converted at use;
  inclusive head with the width-1 minimum, per Helix.
- **Motions partially shared:** char/line/find motions reuse the vim resolve
  engine as planned, but the word motions needed fully Helix-semantic scanners —
  vim's `w` target is simply not Helix's `w` selection.
- **Per-selection registers** (plan's open question) landed as: shared vim
  registers + per-selection multi-yank/paste pairing (`paste_multi`), not a
  full per-selection register file.
- **The `s`-prompt live preview** was unplanned scope that fell out of the
  `incsearch` machinery.
- **Undo/redo of selection placement** was not carried over from placement mode;
  Helix-mode `u`/`U` are document undo/redo (bound by the plugin).

## Follow-on work landed after the initial five phases

- **`r` / `R` / `J`** — replace-char (a pending target-char stage like `f`/`t`),
  replace-with-yank, and join-selected-lines. `R`/`J` are named registry actions;
  `r` is a char-argument grammar key. (`editor/operators.rs`, `editor/helix.rs`.)
- **Match mode (`m`)** — `mm` goto-match, `mi`/`ma` text objects (through the
  shared `resolve_text_object` dispatch — vim objects, tree-sitter captures, and
  `nx.textobject.map` keys alike), and `ms`/`md`/`mr` surround, all
  across **every** selection. A multi-key sub-grammar (`helix_match:
  Option<HelixMatch>`) read raw via `awaiting_command_continuation`. Surround's
  multi-selection edits are placed via a running byte-offset shift
  (`surround_shift` / the ascending `cum` in `helix_surround_add`). `ms` leaves the
  inserted delimiters *inside* each selection; `md`/`mr` restore the *original*
  selection (shifted) rather than jumping to the inner content; and `md`/`mr` accept
  **any** delimiter `ms` can add (a nearest-occurrence scan for non-bracket chars).
  `mr` is two-stage like Helix: `mr{from}` highlights the `{from}` delimiters (the
  live selection becomes them, stashing the real one in `helix_surround_orig`) and
  `{to}` applies the swap and restores the original selection; `<Esc>` mid-preview
  cancels and restores it. `md` stays instant.
- **`X` extend-line-above** — the upward mirror of `x` (`extend_line_above`);
  `helix_extend_line` gained a direction and an order-independent whole-line test.
- **`z` view menu** — `zt`/`zz`/`zb` reposition the viewport around the cursor line
  (a two-key `helix_view` stage reusing the vim `view_reposition`); the selection
  is untouched.
- **`]d`/`[d` (and `]e`/`[e`)** diagnostic navigation, bound in the `helix` keymap
  bucket over `nx.diagnostic.goto_*`, mirroring the vim-mode defaults.
- **`Alt-,` remove-primary-selection** — drops the primary, promoting the next
  selection in document order (the inverse of `,` keep-primary).
- **Register selection (`"{reg}`)** — a two-key `helix_register` stage sets
  `pending.register` (which the vim `yank_range`/`delete`/`paste` already honor), so
  `"ay`/`"ad`/`"ap`/`"aR` target register `a`. It is one-shot: `apply_helix_action`
  clears `pending.register` once the register-reading/writing verb has run.
- **Tree-sitter text objects in match mode** (`mif`/`maf`/`mic`/`mac`/`mia`/…) —
  `helix_textobject` now routes through the shared `Editor::resolve_text_object`
  dispatch (the same one vim's operator/visual paths use), so match mode reaches
  the tree-sitter captures (`f`=function / `a`=parameter / `c`=comment / `t`=class,
  `i`→`.inner` / `a`→`.outer` with the inner→outer fallback and the `count`-th
  enclosing scope) and any `nx.textobject.map` registry key — no longer only the
  vim `&self` object alphabet. One dispatch, so a key can never mean different
  things in vim vs. Helix. This landed for free on web too (the change is in
  `nxvim-core`, over the same engine the vim path uses). Tests:
  `crates/nxvim-server/tests/treesitter_textobjects.rs` (the `helix_*` cases —
  `maf`/`2maf`/`mia`/registry `mig`, sharing the vim suite's real-rust-grammar
  fixture).
- **`Alt-)` / `Alt-(` rotate selection contents** — rotate the *text* among the
  selections (forward = each selection's content moves to the next in document
  order, wrapping; backward the other way), leaving the ranges in place, unlike
  `)`/`(` which move only which selection is primary. `helix_rotate_contents`
  reads each span's text, rotates the vector, replaces descending (so lower byte
  offsets stay valid), and re-fits each selection over its new content through a
  running byte delta (`cum`) — so unequal-width contents rotate cleanly. One undo
  group. Named actions `rotate_selection_contents_forward`/`_backward`.
- **`&` align selections** — pad each selection's start with spaces so every start
  lands on the widest start column (the "align the `=` signs" transform); the
  selection stays on its original content. `helix_align_selections` inserts
  descending and re-places each selection shifted by the cumulative pad. Byte
  columns (exact for ASCII; a preceding wide char offsets the visual column).
  Named action `align_selections`.
- **Per-selection `o` / `O`** — open a fresh line at *every* selection and enter
  multi-cursor Insert (previously primary-only). `helix_open` now reuses the vim
  per-cursor fan-out (`edit_each_cursor(|ed| ed.open_line(below))`) exactly as
  Normal-mode `o`/`O` do — one undo group — matching how `i`/`a`/`I`/`A` already
  enter Insert at every selection. The fresh line moves each head off the line its
  anchor sat on, so leaving Insert **collapses** every selection to a caret at its
  head (`helix_collapse_to_cursor` in the Helix Esc path, `insert.rs`): anchor ==
  head with the marks *kept*, not cleared — a mark-less secondary would make the
  next operator span from its head back to the primary's anchor, since
  `for_each_cursor` only restores each cursor's `visual_anchor` from a present
  anchor mark. (That collapse now also tightens multi-cursor `c`/`i`/`a` exits.)
- **which-key (`nx.on_key_pending`) for the native Helix sub-grammars** — the
  keys-helper popup now lights up mid-sequence for Helix's own multi-key states, not
  just the plugin-mapped menus. The plugin-mapped prefixes (the `g` goto menu, the
  `<Space>` leader) already surfaced via **source A** (the mapped-prefix trie, which
  the oracle reports in any mode). But the *native* sub-grammars — `m` match mode
  (`mm`/`mi`/`ma`/`ms`/`md`/`mr`), `z` view (`zt`/`zz`/`zb`), `f`/`t`/`F`/`T` find,
  `r` replace, `"` register — are driven by `handle_helix`'s own pending fields,
  outside the vim `PendingCommand`, so **source B** (`Editor::command_pending`)
  couldn't see them (and `oracle_mode()` is `None` for Helix). Added
  `Editor::helix_command_pending` (`helix.rs`): the Helix twin of `command_pending`
  that projects the active Helix stage into a `CommandPending` — enumerated
  continuations for the finite prefixes (`m`, `mi`/`ma` reuse the
  `resolve_text_object` alphabet incl. the tree-sitter captures, `z`), a `label` for
  the any-character leaves (find/replace), and the live register list for `"` (reusing
  `register_continuations`). `emit_key_pending` (`effects.rs`) calls it instead of the
  vim `command_pending` when `mode.is_helix()`. Same change **fixed a wart**: the
  built-in-continuation merge in the `Some(kp)` branch is now skipped for Helix, so the
  `g` menu no longer gains stray vim-`g` rows (`gj`/`g#`/tab keys) that don't mean the
  vim thing under Helix. Tests: `key_pending.rs` (the `helix_*` cases). Note: a
  single-key native prefix fed alone settles on the matcher idle-flush, so the tests
  `flush` before reading the event log — the popup's usual after-a-beat appearance.

## Remaining gaps (future work)

- More Helix verbs as demand appears: `C-a`/`C-x` (increment/decrement),
  shell pipes (`|`/`!`), macros (`q`/`Q`), the `[` menu beyond diagnostics.
- `Alt-K`'s remove-prompt shares the keep-prompt's history; Helix separates
  neither — fine, noted.
- Multi-selection `y` moves each head to its range start (an artifact of the
  shared range operator); single-selection `y` keeps the head put (Helix's
  behavior). Harmless for the following `p`, but not pixel-Helix.
