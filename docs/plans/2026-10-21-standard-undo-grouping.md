# Standard undo grouping — undo steps the size a GUI editor uses

*2026-10-21*

Status: **done**. All four phases landed (`22005f8c`, `bb30fa6e`, `27433bc4`,
and the docs commit). See [Outcome](#outcome) for where reality differed from
the sketch. First of two plans: the modeless (GUI/web-only) mode follows this
one and depends on it.

## The gap

bemtvi took vim's undo *grouping*: a whole insert session is a single undo step.
`ihello world, this is a paragraph<Esc>u` takes away everything typed since `i`.
That is the part of vim's undo model that feels wrong. Every other editor the user
works in undoes about a word at a time. It will be unworkable in modeless editing,
where you never leave Insert, so a session covers everything since the file was
opened.

The mechanism lives in one place. `Editor::push_undo` (`editor/undo.rs`) commits
the previous group and opens a new one unless `snapshot_taken` is set.
`enter_insert_each` (`editor/insert.rs`) sets the flag when Insert starts, and the
`<Esc>` arm clears it. Every edit in between lands in the same pending node.

## Decisions (settled with the user)

- **Keep the tree.** `u` / `<C-r>` already walk a linear path. The tree only keeps
  abandoned branches, which `g-`/`g+`, `:earlier`/`:later`, `:undolist` and
  `btv.undotree` use. Nothing about storage, pruning (`'undolevels'`), time travel
  or the undotree projection changes.
- **Change the grouping, for every mode.** Not gated on modeless editing. vim
  users' `u` after a long insert now undoes the last word. That is intended.
- **No option to restore vim grouping.** We can add one later if someone asks;
  until then it is one code path, one behaviour.

## The rules

An edit made while typing joins the pending group unless a **break** happens
first. The break commits the group and opens a new one (the same
`commit_undo` + `mark_dirty` that `push_undo` does), while staying in Insert.

| # | Break when… | Why |
| - | --- | --- |
| 1 | a **word ends**: a non-word char (space, punctuation) is typed right after a word char | the VS Code / Sublime unit |
| 2 | a **newline** is inserted (`<CR>`, `o`-style open) | a line is a natural stopping point |
| 3 | the edit **kind flips**: inserting ↔ deleting (`<BS>`, `<Del>`, `<C-w>`, `<C-u>`) | "undo my deletions" vs "undo my typing" |
| 4 | a **pause**: more than `UNDO_PAUSE_MS` (1000) since the group's last edit | typing in bursts |
| 5 | the cursor **moves by itself**: arrows, `<Home>`/`<End>`, mouse click, `i_CTRL-O` command, jump, while still in Insert | the next edit is somewhere else |

Rule 1 breaks *after* the word ends, before the space is inserted, so the space
belongs to the next group. Undoing `foo bar` gives `foo` (not `foo `), which is
what VS Code does.

Whole operations stay **one step**, and nothing inside them breaks:

- a Normal-mode command, including `c{motion}` plus the text typed after it: the
  `c` deletion and the first word typed form one group, later words break
  normally (the delete is not split from the typed text the way rule 3 would
  otherwise split it);
- a **paste**: `paste_literal`, `<C-r>{reg}`, bracketed paste;
- a **completion accept** (`complete_accept_with`), a **snippet expansion** (and
  every tabstop mirror update), auto-pairs inserting a pair;
- a **multi-cursor** edit: one break check for the whole `for_each_cursor` sweep,
  never per cursor;
- a **replay**: dot-repeat (`replaying_change`), a macro, `:normal` with a range,
  `:g` — `u` after `.` undoes the whole repeated change, like now;
- a **mapping expansion**: an insert-mode map whose RHS types `foo bar` is one
  step, because the user pressed one key. Only the server knows which keys came
  from a mapping (the `Step::Fire` path in `crates/bemtvi-server/src/input.rs`),
  so it has to tell the core. See Phase 3.

Edits from Lua (`btv.buf.set_lines` / `set_text`), LSP workspace edits
(`push_undo_for`) and the `:` command family already open their own groups.
Phase 1 checks they still do and that a break never splits one.

## Implementation sketch

One new core piece: a **group-break check** called by the insert path just
before it mutates.

```rust
// editor/undo.rs
pub(crate) enum EditKind { Insert, Delete }

/// Commit the pending undo group and open a fresh one *without* leaving the
/// insert session, when the next edit should start a new step (word end,
/// newline, kind flip, pause, cursor moved). No-op while a whole-operation
/// group is open (paste, accept, replay, mapping) or outside a typing session.
pub(crate) fn maybe_break_undo(&mut self, next: EditKind, next_char: Option<char>)
```

State it needs on `Editor`, all cleared when the insert session ends:

- `undo_last_kind: Option<EditKind>` (rule 3)
- `undo_last_ms: u64` from `now_ms` (rule 4). `now_ms` is already stamped once per
  message by `dispatch.rs` from the injectable mouse clock, so tests drive it
  with `TestClock`. **Check** that the wasm `EditHost` stamps it too (only the
  `dispatch.rs` call site was seen); if not, stamp it there in Phase 1.
- `undo_last_char: Option<char>` (rule 1)
- `undo_cursor_at: Option<(line, col)>`: where the cursor sat after the last
  edit. Rule 5 compares against it rather than hooking every motion key, so a
  mouse click, a jump or an `i_CTRL-O` command breaks with no extra wiring.
- `undo_atomic: u32`: a depth counter that whole operations raise around their
  body. While it is non-zero, the check does nothing. `snapshot_taken` keeps
  meaning "a group is open"; it is not repurposed.

Each break also refreshes the cursor marks baked into the node
(`refresh_undo_cursor_marks`), so undoing one word puts the cursor back where
that word started, not at the session's start.

## Phases

Each phase lands as its own commit and pauses for review.

### Phase 1 — word, newline and kind breaks

Rules 1–3 plus the whole-operation counter around paste, completion accept,
snippets, auto-pairs, multi-cursor sweeps and replays. This is the change users
will notice. Includes the audit of the existing undo, dot-repeat, macro,
multi-cursor, snippet and Helix suites (`tests/editing/undo.rs`,
`dot_repeat.rs`, `macros.rs`, `multicursor.rs`, `helix_*`, …): any test that
builds a history with a multi-word insert and then undoes it was written to
vim's grouping. Rewrite each one to state the new expectation, never to dodge
it (for example by typing one word).

### Phase 2 — pause and cursor-move breaks

Rules 4–5. Pause tests use `start_clocked` + `TestClock::advance` and never
sleep. Cursor-move tests cover arrows, a mouse click in Insert, and `<C-o>`.

### Phase 3 — mapping expansions are one step

The server marks the keys a `Step::Fire` feeds as one atomic span: raise and
lower `undo_atomic` around them through a narrow core entry point, the same way
paste spans already bracket a key replay. Covers `<expr>` maps and
`btv.keymap.set` with a Lua RHS that feeds keys.

### Phase 4 — docs

`docs/features.md` and the `:help undo` page (if one exists) describe the new
grouping. A short note in the vim-migration skill
(`bemtvi-plugins/bemtvi-plugin-skills`, the `bemtvi-from-neovim` skill) says `u`
is word-sized. Plus an `examples/undo-grouping/` with a spec that types,
pauses (where a spec can drive the clock; otherwise the word, newline and kind
sections only) and undoes.

## Testing

Black-box through the harness, as usual. Each rule gets a test that fails
before its phase and passes after it. Mutation-check each rule by disabling it
and watching its test fail. Run both builds: the default workspace and
`--no-default-features` for the wasm-eligible edit host (Phase 1's clock check
is the one place the two can differ).

Cases to keep in mind:

- `ifoo bar baz<Esc>uuu` → `foo bar`, `foo`, `` (each `u` takes one word)
- `ifoo<BS><BS>x<Esc>u` → `f` then `foo` (delete group, then type group)
- `ifoo<CR>bar<Esc>u` → `foo` + empty line
- `cwnew text<Esc>uu` → `new` after the first `u` (` text` undone), then the
  original word (the `cw` delete and `new` are one group)
- `.` after a multi-word insert, then `u` → the whole repeat undone
- a 3-cursor insert of `a b` → two steps, each spanning all cursors
- paste of `foo bar baz` → one step
- `<C-r>` after each case → redo replays the same groups
- `:undolist` / `btv.undotree` show one node per group, and `g-`/`:earlier`
  walk them.

## Non-goals

- Changing the undo **tree** model, `'undolevels'`, time travel, or the
  persisted shape of anything.
- Restoring the **selection** on undo (VS Code does this). Useful for the
  modeless editing; it is a separate change and belongs to that plan.
- Ctrl-Z / Ctrl-Shift-Z bindings. They belong to the modeless plan's key
  layout.

## Outcome

- **No `undo_atomic` counter for replays.** Dot-repeat, macros, `:normal`, `:g`
  and paste spans are recognised from flags the editor already had
  (`replaying_change`, `macro_executing`, `normal_depth`, `in_global`,
  `paste_active`). The counter exists only for mapping spans (Phase 3).
- **Mapping spans break lazily.** The span's first edit closes the typing step
  before it, not the span's opening, so a mapping that changes nothing doesn't
  split a word. Runs of untyped fed keys are spans too; typed feeds (`t:feed`)
  are not.
- **Register inserts are steps of their own**, like a paste: `<C-r>{reg}`,
  `<C-r><C-w>` and `<C-r>=` all go through `insert_text_session`, which owns
  the before/after hooks.
- **Three bugs predating this work, fixed along the way.**
  - An insert's `<Esc>` closed the step a `:g` or ranged `:normal` batch held
    open, so those undid one line at a time (`undo_batch_depth`).
  - Typing over a snippet placeholder joined the expansion's step, so `u`
    removed the whole snippet (`begin_replacing_step`).
  - The wasm edit host never stamped `now_ms`.
- **Perf.** A commit snapshots the extmark store. With 5000 marks a word-end
  commit costs under 2ms. The guard first seemed to show `<BS>` and separators
  costing ~30ms each in a mark-heavy buffer. The real cost was per *message*:
  any input message carrying two or more keys refreshed every extmark's position
  in the Lua mirror. Fixed separately in the mirror's dirty window.
- **Not done:** the note in the `bemtvi-from-neovim` skill. It lives in the
  separate `bemtvi-plugin-skills` repo.
