# Registers — named, numbered, and special registers

Today nxvim has exactly **one** register: a single unnamed `Register { text,
linewise }` (`crates/nxvim-core/src/editor/mod.rs:117`) that every yank/delete
overwrites and every paste reads. This plan grows that one slot into vim's full
register file: named `"a`–`"z` (with append `"A`–`"Z`), the numbered yank/delete
ring `"0`–`"9`, the small-delete `"-`, the black-hole `"_`, the read-only
specials (`"%` `".` `":` `"/`), and finally the system clipboard (`"+` `"*`).

The unifying syntax is the `"x` register prefix typed before an operator,
paste, or count (`"ayy`, `"Ap`, `"_dd`, `"0p`), plus the `:registers` display
and the `setreg`/`getreg` Lua surface.

## Why this is feasible now (de-risking facts)

Verified in the current tree before planning:

- **There is exactly one register read site and one write site.** Yank writes
  the register in `yank_range` (`crates/nxvim-core/src/editor/operators.rs:193`);
  paste reads it in `paste` (`operators.rs:359`). Every operator
  (`apply_operator_to_range`, the visual operators, `delete_under_cursor`,
  `delete_to_eol`, …) routes through `yank_range` before deleting. So
  redirecting yank/paste to a *selected* register is a change at two chokepoints,
  not a scatter-gun edit. (Confirmed: no other `self.register` references exist.)
- **The command grammar already has clean argument-stage machinery.** `parse_step`
  (`crates/nxvim-core/src/editor/command.rs:372`) threads partial state through a
  `PendingCommand { count, op_count, operator, stage }` with a `Stage` enum for
  "the next key is data" sub-states (`FindPending`, `ReplacePending`,
  `TextObjectPending`, …). Adding `"` as a new `Stage::RegisterPending` + a
  `register: Option<char>` field is exactly the shape this grammar already
  supports — and because the keymap oracle `command_status`
  (`command.rs:628`) folds the *same* `parse_step`, `"a…` prefixes get correct
  disambiguation for free.
- **Core already does dependency injection for impure providers.** The treesitter
  engine is injected as `Option<Box<dyn SyntaxEngine>>`
  (`crates/nxvim-core/src/editor/mod.rs:484`, `syntax.rs:49`), keeping
  `nxvim-core` pure while the server supplies the real implementation. The system
  clipboard (`"+`/`"*`) reuses this exact pattern: core holds an injected
  `Option<Box<dyn Clipboard>>`, the server wires the OS clipboard. No purity
  violation.
- **The Lua compat shims already exist as loud stubs.** `vim.fn.setreg` is a
  `nx._notimpl("vim.fn.setreg")` placeholder
  (`crates/nxvim-lua/src/prelude/fs.lua:275`) — there's a defined seam to fill,
  not a new surface to invent.

## Architecture

### The register file lives in core, keyed by char

Replace the single `register: Register` field with a `registers: Registers`
store (new `crates/nxvim-core/src/editor/registers.rs`):

```rust
struct RegisterCell { text: String, kind: RegKind }   // kind: Char | Line  (Block later)
struct Registers { cells: HashMap<char, RegisterCell>, /* + the numbered ring */ }
```

`Registers` owns the *vim routing rules*, not just storage — these rules are the
real substance of the feature and belong in one pure, well-tested place:

- **`record_yank(reg: Option<char>, text, kind)`** — an explicit `"x` writes
  `x` (and the unnamed `"`); `"X` appends. With no explicit register, a yank
  writes the unnamed `"` **and** `"0` (the "yank register").
- **`record_delete(reg, text, kind)`** — an explicit register behaves like
  yank. With no register: writes unnamed `"`; a delete ≥1 line (or across lines)
  shifts the `"1`–`"9` ring (new text → `"1`, old `"1` → `"2`, …); a small
  (within-line) delete goes to `"-` instead of the ring.
- **`"_` black hole** — `record_*` with `'_'` is a no-op (the delete still
  happens; nothing is stored, and the unnamed register is left untouched).
- **`get(reg) -> Option<&RegisterCell>`** — paste reads the selected register,
  or the unnamed `"` when none is selected.

Keeping every one of these rules inside `Registers` means `yank_range` /
`delete_range` / `paste` just call `record_yank` / `record_delete` / `get` and
stay dumb about routing.

### The `"x` prefix in the grammar

`PendingCommand` gains `register: Option<char>`. In `parse_step`:

1. A `"` key (in `Stage::Start`, normal or visual) → `Prefix` with
   `stage = Stage::RegisterPending`.
2. `Stage::RegisterPending` consumes the next key as the register name, stores
   it in `pending.register`, returns to `Stage::Start`. An invalid name resets
   (loud-by-construction: a dead-end key is `Reset`, matching find/text-object
   aborts).
3. The selected `register` survives the operator-arming step (it carries through
   like `count`), so `"a2dd` and `2"add` both delete two lines into `"a`.
4. `execute_normal` / the operator path read `self.pending.register` and pass it
   into `record_yank` / `record_delete` / `paste`.

`command_status` folds the same `parse_step`, so `"a` reports `Prefix`, `"ay`
reports `Prefix`, `"ayy` reports `Complete` — keymap disambiguation stays
correct with zero extra code (the take-latest harness note in CLAUDE.md is
unaffected; this is pure grammar).

### Read-only specials are computed, not stored

`"%` (current filename), `".` (last inserted text), `":` (last `:` command), and
`"/` (last search pattern) are **projections of existing editor state**, not
cells in the map. `get('%')` reads the buffer name; `get('/')` reads the search
state already held for `n`/`N`; etc. They are read-only — selecting them as the
target of a yank/delete errors loudly (vim: *E354: Invalid register name*),
never silently no-ops.

## Phases

### Phase 1 — Register file foundation (pure, core; unnamed behavior unchanged)

Introduce `Registers` with the routing rules above; replace the single
`Register` field. Yank still writes `"`; paste still reads `"`. **Additionally**
the numbered ring + `"0` + `"-` now populate per the rules — but with no way yet
to *select* a non-unnamed register, this phase is observable only through paste
of the auto-populated registers once Phase 2 lands. So Phase 1 ships *with*
Phase 2's selection to be end-to-end testable; they are split here only for
review clarity.

- New `crates/nxvim-core/src/editor/registers.rs`: `RegKind`, `RegisterCell`,
  `Registers` with `record_yank` / `record_delete` / `get`. The numbered-ring
  shift and small-delete-vs-`"1` decision live here.
- `yank_range` (`operators.rs:193`) → `self.registers.record_yank(reg, text,
  kind)`; `paste` (`operators.rs:359`) → `self.registers.get(reg)`. Thread an
  `reg: Option<char>` param down from the operator/paste callers (defaulted to
  `None` until Phase 2 fills it).
- `delete_range` callers that yank-then-delete (operators, `x`, `D`, …) must
  classify line-vs-char count so `record_delete` can route the ring correctly.

### Phase 2 — `"x` selection in the normal/visual grammar

Wire the prefix end-to-end so the auto-population from Phase 1 becomes reachable.

- `PendingCommand.register: Option<char>`; `Stage::RegisterPending`.
- `parse_step`: `"` arms the stage; the next key fills `register`; carry it
  through operator-arming and `reset_pending`'s boundaries.
- `execute_normal` and the operator/visual-operator paths pass
  `self.pending.register` into yank/delete/paste.
- **Tests** (`editing.rs`): `"ayy` then `"ap` (named round-trip); `"Ayy`
  appends; `dd` then `"1p`/`"2p` (delete ring); a word-delete then `"-p` (small
  delete); `"0p` pastes the last *yank* even after an intervening delete; `"_dd`
  leaves the unnamed register intact (paste after `"_dd` yields the pre-delete
  text); `3"add` / `"a3dd` both capture three lines.

### Phase 3 — `:registers` display + read-only specials

- `:reg[isters]` / `:di[splay]` ex command (dispatch in
  `crates/nxvim-core/src/editor/ex.rs:233`): renders the populated registers as a
  message-area table (`""`, `"0`–`"9`, `"-`, named, then specials), with `^J` for
  newlines as vim does. Optional `:reg ab0` argument filters to listed registers.
- Read-only specials in `Registers::get`: `"%` (buffer name), `"/` (last search
  pattern — reuse the existing search state), `":` (last ex command line), `".`
  (last inserted text — capture the insert-session text on `<Esc>` from insert).
  Selecting any of these as a *write* target errors loudly (E354-style message).
- **Tests**: `:registers` after a yank shows the entry; `"/p` pastes the last
  search; `".p` pastes the last insert; `"%p` pastes the filename; a write to a
  read-only register raises.

### Phase 4 — Lua / RPC surface

- `vim.fn.setreg(name, value, opts)` (fill the stub at `fs.lua:275`),
  `vim.fn.getreg(name)`, `vim.fn.getregtype(name)` — bridged to core
  `Registers` via the Lua install layer, mirroring how other `vim.fn.*` reach
  core state.
- `:put [x]` ex command — paste register `x` (default unnamed) **linewise**
  below the current line, independent of the register's own kind, as vim's
  `:put` does. Lives beside the other ex commands in `ex.rs`.
- **Tests** (`editing.rs`): `setreg('a','hi')` then `"ap`; `getreg`/`getregtype`
  round-trips charwise/linewise; `:put a` inserts a line below.

### Phase 5 — System clipboard (`"+` / `"*`) — DONE

- A `trait Clipboard { fn get(&self) -> Option<(String, bool)>; fn set(&self,
  text: &str, linewise: bool); }` (`crates/nxvim-core/src/clipboard.rs`),
  injected as `Option<Box<dyn Clipboard>>` into the `Editor` exactly like
  `SyntaxEngine`. The boundary uses `bool` linewise (not `RegKind`, which stays
  crate-private) to match the existing public register surface
  (`register_mirror` / `set_register_api`). The `"+`/`"*` cases live in the
  editor (`register_text` for reads, `yank_range`/`delete_yank_range` for
  writes), mirroring how the read-only specials are handled — `Registers` stays
  pure. A clipboard yank mirrors the unnamed register too (vim sets `""` on any
  yank). When absent, `"+` errors loudly (`clipboard: No provider…`) and aborts
  rather than silently using the unnamed register — the operator/visual/paste
  paths all guard on it.
- Server supplies a real provider via **platform shell-out**
  (`crates/nxvim-server/src/clipboard.rs`: `pbcopy`/`pbpaste` on macOS,
  `wl-copy`/`xclip` on Linux), chosen over `arboard` to add no dependency and
  stay lazy (the tool runs only on a `"+` operation). Wired in `run()` next to
  the syntax engine via `ServerInit.clipboard: ClipboardProvider`
  (`System` / `Disabled` / `Custom`); the binary sets `System`, tests default to
  `Disabled` and inject `Custom(fake)`.
- `clipboard=unnamedplus` option — **deferred** (still out of scope; the
  explicit `"+` prefix is the v1 surface).
- **Tests** (`editing.rs`): a fake in-memory `Clipboard` injected via
  `ClipboardProvider::Custom` proves `"+y` → provider, `"+p` ← provider,
  round-trip kind (charwise/linewise), `"*` aliases `"+`, the unnamed mirror,
  and that an absent provider errors loudly (paste *and* delete) without a silent
  fallback. The real macOS shell-out was verified end-to-end out of band (no OS
  clipboard in the suite — that would be faithless/environment-dependent).

### Phase 6 (later) — expression register, blockwise, macros

- **`"=` expression register** — prompts for an expression, evaluates it (via the
  existing synchronous Lua eval used by the statusline plan,
  `eval_to_value_pumped`), and pastes the result. Read-only, eval-on-read.
- **Blockwise registers (`RegKind::Block`)** — requires visual-block mode
  (`<C-v>`), which nxvim does not have yet (`Mode` has only `Visual` /
  `VisualLine`, `mode.rs`). Land with visual-block.
- **Macros `q{reg}` / `@{reg}`** — recording keystrokes into a register and
  replaying them. Reuses the named-register store from Phases 1–2 (a macro is
  just a register holding a key sequence), but the record/replay engine is its
  own feature; tracked separately.

## Out of scope (for now)

- Visual-block-aware (blockwise) register paste — deferred with visual-block mode.
- Macro recording/replay (`q` / `@`) — reuses the store but is a separate engine.
- `:registers` interactive paste UI. (Insert- & command-line-mode `<C-r>{register}`
  has since landed — `<C-r>` arms an `awaiting_register` flag, the next key names the
  register, and `handle_insert` / `handle_command` insert `register_text` at the
  cursor; insert-mode insertion is verbatim, so a linewise register's trailing newline
  splits the line. The `<C-r><C-w>` pseudo-register inserts the word under the cursor
  (`word_under_cursor`, shared with `*`/`#`) instead of a register. Tests:
  `registers::ctrl_r_*`. Still unwired: the `<C-r><C-r>`/`<C-r><C-o>`/`<C-r><C-p>`
  literal/indent variants and `<C-r><C-a>` (WORD) / `<C-r><C-f>` (filename).)
- `clipboard=unnamed` (vs `unnamedplus`) nuance and X11 PRIMARY vs CLIPBOARD
  (`"*` vs `"+`) split — both map to one provider in v1.
