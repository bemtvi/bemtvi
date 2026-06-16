# KeyPending Source B — the built-in command grammar as which-key hints

**Status:** Phase 1 COMPLETE (2026-06-16) — mechanism + all `Stage` labels +
find-char, tested end to end (key_pending + which_key suites, full workspace green).
Phase 2 (enumerated built-in continuations) not started.
**Depends on:** the `nx.on_key_pending` oracle (sources A + C landed); see
`crates/nxvim-server/src/keymap.rs` (`KeyPending`/`Continuation`/`pending_context`)
and `effects.rs::emit_key_pending`.

## Goal

Surface the **core command grammar's** "waiting for the next key" states —
`f`/`F`/`t`/`T` find-char, `r` replace, `i`/`a` text-object, `z`/`g` prefixes,
marks, registers, `<C-w>` — through the same `nx.on_key_pending` event, so a
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

1. **nxvim-core** (`editor/command.rs` + `mod.rs`): a public
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

## Phase 2 (later) — enumerated built-in continuations

Give the finite-set prefixes (`g` → `gg`/`ge`/`g_`/…, `z` → `zz`/`zt`/`zb`/…,
`<C-w>` → window commands) real discrete `continuations` with descriptions, like
sources A/C. Also consider pure operator-pending (`d`/`c`/`y` awaiting a motion)
as an "Awaiting motion" hint.
