# Dot-repeat (`.`) — replay the last change

Today nxvim has no `.` command: pressing `.` in normal mode is a dead-end
(`parse_command`'s final `_ => Reset`). This plan adds vim's single most-reached-for
editing primitive — **repeat the last buffer-changing command**, including the text
typed during an insert session it entered. `dw` then `.` deletes another word; `ciwfoo<Esc>`
then `.` changes the next word's text object to `foo`; `x...` rubs out four characters;
`A;<Esc>` then `j.` appends `;` to the next line.

The approach mirrors vim's own: nxvim records the **raw key stream** of the last
change into a redo buffer and, on `.`, re-feeds those keys through the existing
input path. This is deliberately *not* a structured "record the `ResolvedCommand`"
design — replaying keys reuses the entire grammar (counts, operators, registers,
text objects, find-chars) and the whole insert/replace handler unchanged, so every
normal-mode change that exists today (and every one added later) becomes
dot-repeatable for free, with no per-command bookkeeping to keep in sync. It is the
rust-native analogue of neovim's `AppendToRedobuff` / `start_redo` in `getchar.c`.

## Why this is feasible now (de-risking facts)

Verified in the current tree before planning:

- **There is exactly one key chokepoint, and it already sees post-mapping keys
  one at a time.** Every front end's keystroke reaches the core through
  `Editor::input(key)` (`mod.rs:678`); the server's keymap matcher interposes
  *above* it (`nxvim-server/src/input.rs:28` → `editor.input(key)` at `:98`/`:103`/…),
  so by the time a key lands at `input()` it is already the mapped, literal key.
  Recording there records precisely what should be replayed, and replay can re-enter
  the same `input()`. No new plumbing in the server.
- **The buffer already has a change counter to detect "did this command edit?"**
  `Buffer::changedtick` (`buffer.rs:76`) bumps on every insertion/removal at the two
  choke points (`buffer.rs:281`,`:309`). Snapshotting it at a command's start and
  comparing at its end tells us whether the command was a *change* (commit it) or a
  pure motion (discard it) — without threading a "did I edit" boolean through dozens
  of effect helpers.
- **"Back at a clean command boundary" is already a first-class notion.** The whole
  in-progress command lives in one `PendingCommand` (`command.rs:311`) reset by
  `reset_pending` (`mod.rs:803`) the instant a command completes. A change is done
  exactly when, after handling a key, `self.mode == Normal` **and** the pending state
  is clean — which also correctly spans an insert session (`ciw…<Esc>` is not "done"
  until the `<Esc>` returns to `Normal`, `insert.rs:23`).
- **Re-entrancy is safe.** The core is single-threaded and synchronous; `.`'s handler
  calling `self.input(key)` in a loop is an ordinary nested `&mut self` call. A
  `replaying` guard captured at the top of `input()` keeps the replayed keys from
  recording themselves.
- **`.` is an unclaimed key in the grammar.** `parse_command` (`command.rs:591`) falls
  through `'.'` to `_ => Reset`; `is_register_name` (`command.rs:371`) deliberately
  rejects `.` (the `".` last-insert register is noted as "rejected until its phase
  lands"). Adding `NormalCmd::DotRepeat` collides with nothing.

## Architecture

### A redo buffer of keys, recorded at `input()`

Three fields on `Editor` (`mod.rs`):

```rust
/// Keys accumulated since the current normal-mode command boundary — the
/// in-progress candidate change. Committed to `last_change` when the command
/// finishes having edited the buffer; cleared when it finishes without editing.
redo_recording: Vec<Key>,
/// The committed last change, replayed verbatim by `.`. Empty until the first
/// buffer-changing normal-mode command.
last_change: Vec<Key>,
/// True while `.` is re-feeding `last_change`, so the replayed keys neither
/// record themselves nor overwrite `last_change`.
replaying_change: bool,
```

plus two per-window scratch fields reset at each boundary: `change_start_tick: u64`
(the `changedtick` when the current command began) and `change_not_repeatable: bool`
(set when the command is one `.` must *not* capture — see below).

A small predicate distinguishes a clean boundary. `PendingCommand` derives only
`Default`/`Clone`, so add:

```rust
impl PendingCommand {
    /// At a clean command boundary: no count, operator, register, or argument
    /// stage pending — the next key starts a fresh command.
    pub(crate) fn is_clean(&self) -> bool {
        self.count.is_none() && self.op_count.is_none()
            && self.operator.is_none() && self.register.is_none()
            && self.stage == Stage::Start
    }
}
```

### The recording wrapper in `Editor::input`

`input()` (`mod.rs:678`) wraps its existing mode-dispatch `match` (`mod.rs:696`) —
*after* the panel and `:s///c`-confirm early-returns, which are not buffer changes:

```rust
let recording = !self.replaying_change;          // snapshot: outer call records
let starting = recording
    && matches!(self.mode, Mode::Normal | Mode::Visual | Mode::VisualLine)
    && self.pending.is_clean();
if starting {
    self.redo_recording.clear();
    self.change_start_tick = self.buffer().changedtick;
    self.change_not_repeatable = false;
}
if recording {
    self.redo_recording.push(key);
}

// … existing `match self.mode { … }` dispatch …

if recording {
    // Entering the command line (`:`/`/`/`?`) makes this window non-repeatable:
    // `:d`, `:s`, and operator-search `d/foo` are not `.`-repeatable in vim.
    if self.mode == Mode::Command {
        self.change_not_repeatable = true;
    }
    let done = self.mode == Mode::Normal && self.pending.is_clean();
    if done {
        let changed = self.buffer().changedtick != self.change_start_tick;
        if changed && !self.change_not_repeatable {
            self.last_change = std::mem::take(&mut self.redo_recording);
        }
        self.redo_recording.clear();
    }
}
```

This single block handles every case uniformly:

| Keystrokes | Trace | Result |
|---|---|---|
| `x` | start→push→delete→done, changed | commit `[x]` |
| `dw` | `d`: start, pending→operator (not done); `w`: push, exec, done, changed | commit `[d,w]` |
| `ciwfoo<Esc>` | `c`/`i`/`w` enter insert (not done); `f`/`o`/`o` in Insert (not done); `<Esc>`→Normal, done, changed | commit the whole run |
| `w` (motion) | start→move→done, **un**changed | discard; `last_change` kept |
| `u` / `<C-r>` | arm sets `change_not_repeatable` | discard (see below) |
| `:d<CR>` | mode passes through `Command` → non-repeatable | discard |

### `.` in the grammar and its handler

- `parse_command` (`command.rs:662` match): `'.' => Complete(RC::Normal(N::DotRepeat))`.
- `NormalCmd::DotRepeat` variant; `execute_normal` arm (`command.rs:912`):

```rust
NormalCmd::DotRepeat => self.repeat_change(),
```

```rust
fn repeat_change(&mut self) {
    if self.last_change.is_empty() {
        return;                       // nothing to repeat (vim beeps; nxvim has no bell)
    }
    // `.` itself must never become the new last change, so subsequent `.`
    // replay the *original* — mark this window non-repeatable.
    self.change_not_repeatable = true;
    let keys = self.last_change.clone();
    self.replaying_change = true;
    for key in keys {
        self.input(key);             // re-enter: guard skips recording for these
    }
    self.replaying_change = false;
}
```

The recursive `self.input` calls run with `replaying_change == true`, so their
`recording` snapshot is `false` and they skip the whole record/commit block — they
only *execute*. Because `last_change` is committed only for buffer-changing,
non-command-line, non-undo commands, it can never contain `.`, `:`-driven edits, or
`u`/`<C-r>`, so replay can't recurse or resurrect a non-change.

### Exclusions (the commands `.` must not capture)

Three commands edit the buffer but are **not** the dot-repeat target in vim; each
sets `change_not_repeatable` so the commit is skipped:

- **`u` / `<C-r>`** — set the flag in the `Undo`/`Redo` arms of `execute_normal`
  (`command.rs:940`). (Undo restores text and bumps `changedtick`, so without this
  flag `u` would wrongly become the last change.)
- **Anything routed through the command line** — `:d`, `:s`, `:normal`, and the
  operator-pending search motions `d/pat<CR>` / `c?pat<CR>`. Caught centrally by the
  `self.mode == Mode::Command` check in the wrapper (the command transits `Command`
  mode). `:s` repeats with `&`/`@:`, not `.`, matching vim.

## Phases

### Phase 1 — record + replay normal-mode changes (the core)

End-to-end `.` for changes initiated in normal mode, replayed verbatim (the
recorded count, register, operator, motion, and inserted text all re-parse from the
keys).

- `redo_recording` / `last_change` / `replaying_change` / `change_start_tick` /
  `change_not_repeatable` on `Editor` (`mod.rs`); `PendingCommand::is_clean()`.
- The recording wrapper in `Editor::input`; the `change_not_repeatable` flag set in
  the `Undo`/`Redo` arms.
- Grammar: `'.'` in `parse_command`; `NormalCmd::DotRepeat`; `repeat_change()`.
  Because the dispatch is the typed `ResolvedCommand`, the keymap oracle
  `command_status` recognizes `.` as a complete command for free (no take-latest
  harness interaction — pure grammar).
- **Tests** (`crates/nxvim-server/tests/editing.rs`, the black-box harness — feed
  notation via `nvim_input`, assert `nvim_buf_get_lines`/cursor):
  - `x` then `.` deletes two chars; `3x` then `.` deletes 3 + 3.
  - `dw` then `.` deletes a second word; `dd` then `.` deletes a second line.
  - `ciwfoo<Esc>` on `one two` then `w.` → `foo foo`.
  - `A;<Esc>` then `j.` appends `;` to the next line (insert text replays).
  - `rx` then `l.` replaces two chars; `~` then `.` toggles two.
  - `p` then `.` pastes twice.
  - A pure motion (`w`) between a change and `.` does **not** change what `.` repeats.
  - `u` is not repeated by `.` (after `xu`, `.` repeats the `x`, not the undo);
    `:d<CR>` is not repeated by `.`.
  - `.` with no prior change is a no-op (buffer unchanged).

### Phase 2 — `[count].` count override

In vim a count on `.` *replaces* the original command's count: `dw` then `3.` does
`3dw`; `3x` then `.` still deletes 3 (no new count → reuse recorded).

- In `repeat_change`, when `effective_count()` came from an explicit count on the
  `.` (track via `self.pending.count.is_some()` before `reset_pending`), strip the
  recorded command's **leading** count keys from `last_change` and prepend the new
  count's digits before replaying. Leading-count extraction is a tiny scan of the
  recorded `Key`s (ASCII digits at the front, not a `0` in first position — the same
  rule `parse_step` uses at `command.rs:498`).
- **Tests**: `dw` then `3.` deletes three words; `2dd` then `.` deletes two lines;
  `2dd` then `3.` deletes three; an explicit `0` is treated as a motion, not a count.

### Phase 3 (later) — the honest deferrals

Recorded but deliberately scoped out of Phase 1, each noted to stay honest rather
than silently approximated:

- **The `".` last-insert register.** The insert session's keys are already captured;
  `".` wants the literal inserted *text*. A small projection (replay the insert keys
  into a scratch string, or capture inserted chars during the session) fills the
  register `is_register_name` currently rejects (`command.rs:371`). Natural follow-on
  since the recording already exists.
- **Faithful visual-mode dot-repeat.** A change begun in visual mode (`viwd`, `Vjd`)
  records and *will* replay its keystrokes, but vim's visual `.` reselects the same
  **size** (same line/char count) from the new cursor rather than re-running the same
  motion keys. Phase 1 commits visual-initiated changes as plain key replay (a close
  approximation); making it size-faithful (stash the last visual extent, synthesize
  the reselect) is its own step. Until then the divergence is documented at the
  recording site, not hidden.
- **Operator + search motion (`d/pat<CR>` then `.`).** Excluded by the command-line
  rule above (it transits `Command` mode). Repeating it faithfully means recording
  the committed search pattern alongside the keys; deferred with the rest of the
  command-line-driven changes.

## Out of scope (for now)

- Recording across an `nvim_input` that spans *multiple* changes in one RPC call —
  each change boundary commits independently, which is already correct; no batching
  semantics beyond that.
- `q`/`@` macros — a separate recording engine (registers Phase 6); the redo buffer
  here is single-change, not a named, user-controlled recording.
- The `g.` / `gi` family and `:normal .` — trivial once the redo buffer exists, but
  not needed to exercise the feature.
