# KeyPending Source B — the built-in command grammar as which-key hints

**Status:** Phases 1–4 COMPLETE (2026-06-16). Phase 1 — mechanism + all `Stage`
labels + find-char. Phase 2 — enumerated `g`/`z`/`<C-w>`/`<C-w><C-w>` continuations
+ the A+B merge for shared prefixes (`g` + the LSP defaults) + the `available` flag
for timed-out maps. Phase 3 — the remaining built-in states (operator-pending
motions, text objects, registers, marks) + descriptive `keys — label` titles.
Phase 4 — per-segment inline float highlighting: `btv.ui.float` content lines may now
be chunk runs (`{ {text, hl_group}, … }`), so the which-key example colours keys vs.
group labels vs. descriptions and DIMS timed-out maps (no more `(×)` cue). Tested end
to end (key_pending + which_key + ui_float suites, full workspace green).
**Depends on:** the `btv.on_key_pending` oracle (sources A + C landed); see
`crates/bemtvi-server/src/keymap.rs` (`KeyPending`/`Continuation`/`pending_context`)
and `effects.rs::emit_key_pending`.

## Goal

Surface the **core command grammar's** "waiting for the next key" states —
`f`/`F`/`t`/`T` find-char, `r` replace, `i`/`a` text-object, `z`/`g` prefixes,
marks, registers, `<C-w>` — through the same `btv.on_key_pending` event, so a
native which-key shows e.g. **"Find character"** when the editor is mid-`f`.

Motivated by the find-char swallow in
`memory/whichkey-timeout-replay-is-neovim-faithful.md`: rather than diverge from
neovim, make the pending state *legible*.

## The shape difference (drives the API)

Sources A/C enumerate **discrete** continuations (`q quit`, `w write`). The
built-in leaf states have an **open continuation set** — find-char takes *any*
printable char, marks/registers any letter — so there's nothing finite to list.
They need a **context-level label** instead.

→ Additive schema bump: `ctx.label` (a string, or `nil`). Sources A/C leave it
`nil` and keep working untouched. which-key renders `ctx.label` when
`continuations` is empty.

## Precedence

At any instant the matcher is *either* withholding a mapped prefix (A/C) *or*
has released keys to the editor which left it mid-command (B) — never both (a
withheld prefix hasn't reached the editor yet). So: compute A/C first; if it's
`None`, fall back to the editor's command-pending state. One unified
`KeyPending` flows through the existing `last_key_pending` change-detection, so
A↔B transitions fire correctly and the cleared event still closes the popup.

## Phase 1 (this commit) — mechanism + all `Stage` variants, find-char flagship

1. **bemtvi-core** (`editor/command.rs` + `mod.rs`): a public
   `CommandPending { label: &'static str, keys: String }` and
   `Editor::command_pending() -> Option<CommandPending>`, `Some` whenever
   `pending.stage != Start`. `keys` is the showcmd-style prefix typed so far
   (count + register + operator + stage trigger, e.g. `2"adf`); `label` maps the
   `Stage` variant to a hint:
   - `FindPending(f/t/F/T)` → "Find character" / "Find char backward" /
     "Till character" / "Till char backward"
   - `ReplacePending` → "Replace character"
   - `TextObjectPending` → "Text object"
   - `ZPending` → "z — scroll / fold", `GPending` → "g commands"
   - `RegisterPending` → "Register", `MarkSetPending` → "Set mark",
     `MarkJumpPending` → "Jump to mark"
   - `WindowPending` → "Window command", `WindowLayerPending` → "Dock layer"

   Every variant maps to a real label (no stub). Finite-set states
   (`g`/`z`/`<C-w>`) get a label but no enumerated continuations yet — that's the
   Phase 2 follow-up.

2. **Schema bump**: `KeyPending` gains `label: Option<String>` (A/C set `None`).
   `run_key_pending` gains a `label` param → sets `ctx.label`. Document in
   `prelude/keymap.lua`.

3. **Server** (`effects.rs::emit_key_pending`): when `pending_context` is `None`,
   build a `KeyPending` from `editor.command_pending()` (mode = the editing
   scope's code, `continuations = []`, `label = Some`).

4. **which-key example**: render `ctx.label` (centered, dim) when there are no
   continuations — so `f` shows a "Find character" card.

5. **Tests**: `key_pending.rs` — find-char fires `label` with the right `keys`
   and clears on the target; operator composition (`df`); a non-find stage
   (`r`); A→B transition (a leader map's `f`-group times out → find-char hint).
   `which_key.rs` — the label card renders.

## Phase 2 (DONE 2026-06-16) — enumerated built-in continuations

Give the finite-set prefixes real discrete `continuations` with descriptions, like
sources A/C; surface operator-pending as an "Awaiting motion" hint.

1. **bemtvi-core** (`command.rs`): `CommandPending` gains `continuations:
   Vec<CommandContinuation>` (`{ key, desc, group }`). `command_pending` is now a
   pure `pending_hint(&PendingCommand)`; the finite stages get curated lists built
   beside the grammar that resolves them (`z_continuations` ↔ `view_command`,
   `window_continuations` ↔ `window_command`, `window_layer_continuations`, and the
   `g`-prefix arm → `g_continuations` with `` g` ``/`g'` as groups). Only the
   *intentional* commands are listed — the accidental `parse_command` fall-throughs
   (`gu`, `gp`) are not advertised. Operator-pending (`Stage::Start` with an
   operator) now returns the "Awaiting motion" label.

2. **The A+B merge.** `g` is *always* a withheld source-A prefix (the LSP
   `gd`/`gD`/`gr` native defaults), so the built-in `g`-motions would be invisible
   under the Phase-1 "A *or* B" precedence. New pure `command_pending_after(mode,
   keys)` folds a key run hypothetically and returns the hint for the carried
   prefix — but **only** when the whole run is a single uninterrupted built-in
   prefix (it bails on any mid-run `Complete`, so a `<Space>g` leader group never
   mis-merges `g`-motions). `effects.rs::emit_key_pending` merges those built-in
   continuations into the withheld source-A context (deduped — a user map on the
   same key wins — and re-sorted by key). `z`/`<C-w>` have no native-default map, so
   they reach the editor and the existing source-B path carries their list directly.

3. **Tests/example/docs:** `key_pending.rs` (+ z/`<C-w>`/`<C-w><C-w>`/operator-
   pending/g-merge/user-map-wins) and `which_key.rs` (the `z` grid renders); the
   example's header documents the built-in prefixes; `native_default` test rewritten
   for the merged `g`.

### Not enumerated (deliberate)

`<C-w><C-w>` lists only the directional layer-crosses (`h`/`j`/`k`/`l` cross,
`H`/`J`/`K`/`L` move) — the cross-then-window combos (`<C-w><C-w>v`, …) work but
would bury the layer ops, so the card stays focused. (Operator-pending, text
objects, registers, and marks were enumerated in Phase 3; only find-char and replace
— which take *any* character — stay label-only.)

### Timed-out maps stay visible (the `available` flag)

`g` is *always* a withheld source-A prefix, so pressing it shows the merged list
(maps + built-ins). After the leader **timeout**, the idle flush commits `g` to the
built-in grammar (faithful to neovim's timeout model) — and from then on `gd`/`gD`/
`gr` can no longer fire (typing `d` runs the built-in operator). The Phase-2 source-B
event would *drop* them, so they vanished too fast to read. Fix: each continuation
now carries `available: bool` (`keymap::Continuation` / the Lua payload). In the
post-timeout source-B `g`-state the server keeps the trie's `g`-maps in the list via
`Keymaps::continuations_at`, flagged `available = false` (deduped against the
available built-ins). which-key keeps them visible. Tested:
`timed_out_g_maps_stay_listed_as_unavailable`.

## Phase 3 (DONE 2026-06-16) — the remaining built-in states + descriptive labels

Two things made the popup cryptic: open-set states showed only a bare label (`d` →
"Awaiting motion"), and the prefix key itself was unlabelled. Phase 3 fixes both.

1. **Descriptive labels.** `label` now names the command in human terms — operators
   `d`/`c`/`y`/`=` → Delete/Change/Yank/Indent (`operator_name`), `g` → "Go", `z` →
   "Scroll / fold", `<C-w>` → "Window". The example titles the popup `keys — label`
   (" d — Delete "), so no bare key.

2. **Enumerate the rest.** The states that *looked* open are actually finite:
   - **Operator-pending** (`d`/`c`/`y`/`=`): `operator_motion_continuations(op)` — the
     operator-range alphabet (word/line/linewise/goto/char motions complete the
     range; find / text-object / `g` / mark / search are groups that arm a further
     stage; the doubled operator is "current line(s)"). Static, beside the grammar.
   - **Text objects** (`i`/`a`): `text_object_continuations()` — the
     `ObjectKind::from_key` alphabet (word, the bracket pairs, quotes, paragraph,
     sentence). Static. (`t`/tag is *not* listed — `from_key` doesn't implement it.)
   - **Registers** (`"`): `Editor::register_continuations` — the registers that
     actually hold text (`Registers::entries()`), keyed to a one-line content
     preview (`preview_text`). Not the bare a–z alphabet. Once a register is
     *selected* (`"a`, grammar back at `Start` with the register armed), the hint
     does *not* close — it shows label "Use register" (the name is in `keys`) and the
     actions that consume it (`register_action_continuations`: `p`/`P`/`x` complete,
     `d`/`c`/`y` are groups awaiting a motion).
   - **Marks** (`` ` ``/`'`/`m`): `Editor::set_mark_continuations` — the marks
     actually set, not the 52-letter alphabet. Every row leads with the position
     (`{line}:{col}`); a read-only automatic mark (`'`/`.`/`^`/…) shows its *meaning*
     (`special_mark_name`: "previous position", "last insert") rather than its line —
     otherwise a `'` mark on a comment read as a mystery snippet — while a named mark
     shows a line preview and a global `A`–`Z` shows its file.

   The dynamic states (registers, marks) read live editor state, so `command_pending`
   (not the pure `pending_hint`) enriches them; `CommandContinuation.desc` became
   `String` for the previews. Only find-char (`f`/`t`/`F`/`T`) and replace (`r`) stay
   label-only — they take *any* character.

   Tested: `key_pending` (operator motions / label, text objects, register contents,
   set marks) + `which_key` (`d — Delete` titles and lists motions). The operator
   motion list overflows the popup on 80×24 — the example is single-column; a
   columned/paged layout is a `btv.ui.float` capability question, see Phase 4.

## Phase 4 (DONE 2026-06-16) — inline float highlighting for a "pretty" which-key

`btv.ui.float` content was plain `Vec<String>` lines rendered single-style (only the
selectable-list `Menu` had per-row highlight). So the which-key example could only
*text-cue* an unavailable row (a trailing `(×)`), not truly **gray** it — and more
broadly a which-key couldn't colour keys vs. descriptions, group `+prefix` labels.

**What landed — per-SEGMENT highlighting, reusing the `virt_lines` machinery.** A
content-float line is now a **chunk run** (`Vec<Vec<VirtChunk>>`), the same shape
`virt_text`/`virt_lines` already use, rather than a bare `String`. Each chunk is
`{ text, hl_group? }`; the server resolves `hl_group` → a per-frame style id and
ships `[[text, style_id], …]` (the existing `virt_chunks_value` + `StyleTable`), so a
renderer that already paints `virt_lines` style ids needs almost no new code.

Threaded: `btv.ui.float` (Lua `float_lines` normalizes a string row, a chunk-list row,
or a mix) → `_ui_float` (`Vec<Vec<VirtChunkData>>`, parsed by the existing
`virt_chunks_from_table`) → `UiFloatReq` → `effects.rs` lowers to `VirtChunk` →
`Editor::open_styled_float` → `ContentFloat`/`ContentFloatView`
(`Vec<Vec<VirtChunk>>`) → `project_content_float` (geometry from summed chunk widths;
emits chunk runs) → `ContentFloatData` + `parse_float_lines` (bemtvi-view) → the three
renderers (TUI `content_float_line` via `rt`/the palette, GUI `Seg` runs, web per-
chunk `<span>` + `styleToCss`). A plain caller (LSP hover/signature via
`open_content_float(Vec<String>)`) becomes one un-styled chunk per line → `Nil` style
id → normal colors, so nothing regressed.

The which-key example now defines `WhichKey`/`WhichKeyGroup`/`WhichKeyDesc`/
`WhichKeyDim` groups and emits chunked rows: keys cyan, `+prefix` groups purple-bold,
descriptions light, and timed-out `available == false` maps **dimmed** (the `(×)`
text cue is gone — the colour carries it). Tested: `ui_float`
(`styled_chunk_lines_carry_per_segment_highlights`) and `which_key`
(`rows_colour_keys_groups_and_descriptions`, `timed_out_g_map_row_is_dimmed_not_text_cued`).
