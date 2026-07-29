# `BufWinEnter` — neovim semantics

**Date:** 2026-07-28
**Status:** both phases landed, plus the review follow-ups at the end. Deviations from
the plan, and why, are recorded inline under each phase.

## Problem

Two defects, one report. nxvim fires `BufWinEnter` on every tab switch (neovim never
does), and its underlying model — "a buffer's window-visibility went 0 → ≥1" — is not
neovim's, so it also misses fires neovim makes.

### Measured (nvim 0.12.2 headless vs. nxvim through the harness)

| action | neovim | nxvim (today) |
| --- | --- | --- |
| `:tabnext` / `:tabprevious` | no fire | **fires every switch** |
| `:tabclose` returning to another tab | no fire | **fires** |
| `:split b.txt` / `:tabnew b.txt` when `b.txt` is shown elsewhere | fires | **no fire** |
| `:e!` reload of the displayed buffer | fires | **no fire** |
| `:b other` then `:b back`, single window | fires | fires |
| `:split` no args, `<C-w>w` | no fire | no fire |
| background window filled by a session restore | fires | fires |

The tab-switch defect is not confined to `BufWinEnter`. The whole lifecycle window diff
sees only the active tab, so one `:tabnext` fires four spurious events:

```
neovim :  WinEnter TabEnter BufEnter
nxvim  :  WinNew WinClosed BufEnter WinEnter BufWinEnter WinResized TabEnter
          ^^^^^^ ^^^^^^^^^                  ^^^^^^^^^^^ ^^^^^^^^^^ spurious
```

### Root causes

1. **Active-tab-only enumeration.** `EditHost::emit_lifecycle_events` builds its window
   set from `Editor::window_ids()` (`crates/nxvim-core/src/editor/windows.rs:1254`),
   which walks `layer_tree(layer)` — the **active tab** of each layer
   (`editor/dock.rs:127`). Windows parked in background tabs are invisible, so leaving a
   tab reads as "those windows closed" and returning as "these windows are new".
   `Editor::all_window_ids()` (`editor/tabs.rs:103`, already backing `nvim_list_wins`)
   is the cross-tab enumeration this diff should have used.
2. **Wrong model.** `BufWinEnter` is derived from a per-*buffer* visibility edge
   (`lifecycle.rs:741`). Neovim fires it from the buffer load/switch paths — `open_buffer`
   (`buffer.c:433`), `do_ecmd`'s already-loaded branch (`ex_cmds.c:2833`), `enter_buffer`
   (`buffer.c:1821`), plus `:recover` and the quickfix window — and from nothing in
   `window.c`, which is why tab and focus navigation never fire it and why a *second*
   window displaying an already-displayed buffer does.

   Neovim's own `:h BufWinEnter` still claims `:split` with a file already open in a
   window doesn't trigger. That note is stale — `do_ecmd` fires unconditionally, and
   `:vsplit a.txt` / `:tabnew a.txt` with `a.txt` already displayed both fire on 0.12.2.

## Phase 1 — the lifecycle window diff spans every tab

Swap the lifecycle diff's enumeration from `window_ids()` (active tab of each layer) to
`all_window_ids()` (every tab of every open layer). Windows in background tabs then stay
continuously known, so a tab switch transitions nothing.

Touchpoints, all in `crates/nxvim-server`:

- `lifecycle.rs::emit_lifecycle_events` — the `wins` vec feeding `new_wins` /
  `closed_wins` (`WinNew` / `WinClosed`), `bufwin_changed`, and `known_windows`.
- `lifecycle.rs::window_rects_snapshot` / `window_scroll_snapshot` — `WinResized` /
  `WinScrolled` baselines; the rect vec differing by *membership* on every tab switch is
  what fires the spurious `WinResized`.
- `lib.rs:1775` and `lib.rs:3838` — the two startup seeding sites, which must seed the
  same set or the first diff reads every background-tab window as new.

The doc-float filter (`is_doc_float_window`) is preserved at each site. `window_buffer` /
`window_rect` / `window_scroll` already resolve parked windows through
`any_tab_tree_of_window`, so no core read needs widening.

Knock-on (intended): `announce_displayed_buffers` walks `known_windows`, so buffers
restored into background *tabs* now get their `BufReadPost` → `FileType` announce like
background *windows* already do — matching neovim, which loads every restored tab's
buffers.

Tests (`crates/nxvim-server/tests/autocmds.rs`), written failing first:

- `tab_switch_fires_no_window_lifecycle_events` — with two tabs, `:tabnext` fires exactly
  neovim's `WinEnter` / `TabEnter` / `BufEnter` and none of `WinNew` / `WinClosed` /
  `WinResized` / `BufWinEnter`.
- `tabnew_and_tabclose_still_fire_win_new_and_closed` — the guard that phase 1 suppresses
  only the spurious pairs, not real window creation/destruction.

**Landed, plus two core gaps the plan hadn't costed.** Widening the enumeration was not
enough on its own; both of these are the "a read keyed off `tree_of_window` can't see
another tab's window" class that `any_tab_tree_of_window` exists to close:

- `Editor::window_rect` resolved only the active tab, so the `WinResized` snapshot
  flipped every background window to `(0,0,0,0)` and back across a switch. Widened to
  `any_tab_tree_of_window`.
- `Editor::relayout` laid out only each layer's *active* tab, so a parked tab kept the
  rects it had when last focused and caught up on switch-in — which the rect diff then
  read as a real resize (the tabline appearing on `:tabnew` is enough to trigger it).
  It now lays out every tab, as neovim resizes every tabpage; `nvim_win_get_height` on a
  background-tab window stops reading stale as a side effect.

## Phase 2 — per-window display assignment, not per-buffer visibility

Restate the rule as neovim's: **a window is now displaying a buffer it was not displaying
before**, tracked per window rather than per buffer.

`known_window_buffers` (now covering every tab, from phase 1) becomes the baseline:

1. **Existing window whose buffer changed** → fire for the new buffer. Catches `:b x`,
   `:e file`, `nvim_win_set_buf`, a restore filling a background window — and fires once
   per window, so a second window showing an already-displayed buffer fires (today it
   does not).
2. **Newly created window** → fire unless it *inherited* its buffer from the window it
   was split from. A plain `:split` / `:vsplit` inherits and must not fire; `:split
   b.txt` inherits and is then reassigned within the same tick, so the diff sees
   `a → b` and fires; `:tabnew` mints a fresh buffer and fires.
3. **A displayed buffer re-read from disk** → fire. This is `:e!` / a reload / a
   deferred `:edit` reusing the throwaway `[No Name]` id in place, where no window
   changed which *buffer id* it holds. The signal already exists:
   `Editor::take_loaded_in_place()`, which the announce path uses for the same reason.

Rule 2 needs one canonical core signal — which new windows inherited. A heuristic
("its buffer matches some other window's") is wrong: `:vsplit a.txt` with `a.txt` already
displayed must fire. So the split path records the fact, mirroring the
`loaded_in_place` precedent:

- `crates/nxvim-core/src/editor/mod.rs` — `inherited_windows: Vec<(WindowId, BufferId)>`.
- `editor/windows.rs::split` (`:split` / `:vsplit`, the only pure-inherit creation path;
  `open_split_window` takes an explicit buffer and is an assignment) pushes
  `(new_id, buffer)`.
- `Editor::take_inherited_windows()` — drained by the server, which seeds those windows'
  baselines before diffing.

Ordering and gating are unchanged: still gated on a registered handler, still sequenced
behind an in-flight read chain via `ReadChain::deferred_win_enter` so the order stays
`BufReadPost` → `FileType` → `BufEnter` → `BufWinEnter`.

Known deliberate divergences to verify against nvim during the phase, and suppress only
if they over-fire: `:bdelete`'s sweep rebinding background windows off the freed buffer
(`buffers.rs:2911`) and `<C-w>x`'s buffer-field exchange (`windows.rs:2622`).

Tests: the table at the top of this document, one test per row, plus the two existing
`BufWinEnter` tests (which must keep passing unchanged — both describe behavior the new
model preserves).

Docs: `docs/autocmd-events.md:107` states the visibility model and must be rewritten to
the assignment model.

**Landed as designed.** Three findings changed the edges:

- **`<C-w>x`** fires nothing, matching nvim. **`:bdelete`** fires in nxvim and not in
  nvim, but the two aren't comparable: nvim *closes* the window showing the deleted
  buffer (2 windows → 1, `BufEnter` only), while nxvim keeps it and rebinds it to a
  survivor — a window genuinely displaying something new, which under this model fires.
  A suppression was written and reverted: it would only have covered *background*
  windows, leaving the current window (which moves via the ordinary switch path) firing
  on the same command. nxvim's `:bdelete` keeping the window open is a real divergence
  from vim, and its own question.
- **`:b <name>`** routes focus to a window already showing that buffer instead of
  switching in place (nxvim's `:drop`-like behavior, a divergence from vim's `:b`), so
  it changes no window's buffer and correctly fires nothing. The switch test therefore
  drives one window with `:edit` / `:b` on buffers displayed nowhere else.
- **Bare `:e` / `:e!` did not reload at all** — only `:e ++enc=…` fell back to the
  current file name, so the plain form answered `E32: No file name` on a well-named
  buffer and left unsaved changes in place. Fixed in `ex_edit` (`E32` now means the
  buffer really has no name), with its own test in `tests/buffers.rs`. The `:e!` row of
  the table above is unreachable without it.

## Review follow-ups

Re-probing the finished work against nvim 0.12.2 — the same three events logged through
one script on each editor, `BufLeave` / `BufEnter` / `BufWinEnter` over twelve commands —
turned up three gaps the two phases left. All three are the same shape as the bug phase 2
fixed: a *diff* cannot see an event whose cause moved nothing.

- **The remote tier never fired the re-read `BufWinEnter`.** Phase 2 hung it off
  `Editor::take_loaded_in_place`, which only the two *synchronous* read paths record
  (`load_into_current`, `load_pending_open`). A daemon / wasm read lands in the server
  (`EditHost::load_replica_bytes`), which cleared the `announced` / `fired_filetype` /
  `fired_encoding` sets by hand instead — so `BufReadPost` and `FileType` re-fired over
  the wire but `BufWinEnter` did not, and neither did the `BufEnter` below. The landing
  now reports the fact (`Editor::mark_loaded_in_place`) and the one drain does the rest,
  so the tiers share a path rather than a near-copy that drifts. Guarded by
  `reload_over_the_wire_fires_bufwinenter` in `tests/daemon_edit.rs`, which also pins the
  dedup: an open that moves the window fires **once**, not once per signal.
- **A re-read fired no `BufEnter`.** `:e!` logged `BufReadPost, BufWinEnter` against
  nvim's `BufReadPost, BufEnter, BufWinEnter` — `entered` was derived purely from the
  current buffer *id* changing, which a reload by definition doesn't do. Restated as
  neovim's `do_ecmd`: a re-read re-enters what it re-read. No `BufLeave` (nothing was
  left), which is nvim's behavior too.
- **`BufLeave` fired after the incoming buffer's read.** `:edit other` logged
  `BufReadPost, BufLeave, BufEnter` against nvim's `BufLeave, BufReadPost, BufEnter`, so
  a plugin's "save this buffer's state on the way out" handler ran *after* the arriving
  buffer's `BufReadPost` had restored state for the new one — the two halves of one
  plugin, inverted. The fire is hoisted above the announce; its `BufEnter` twin stays on
  the far side of the read chain, which is what the chain orders.

- **`:split <the file you are already editing>` re-read it from disk.** Verifying the
  example turned this up: `:split file` is a split *then* `:edit file`, and with no
  argument-vs-inherit distinction the `:edit` took its "re-edit the current file" branch.
  vim takes `do_ecmd`'s old-buffer path — the window already shows it, so nothing is read
  — and nxvim's reload cost two things a request for a second view has no business
  costing: on a modified buffer the reload's `E37` guard *refused the split*, and on a
  clean one the fresh read re-rooted the undo tree, so every undo step taken before the
  split was gone. `ex_edit_file` now consumes the window's inherit record instead, which
  is also what makes the split a *display* — so `BufWinEnter` still fires, and the phase-2
  test that covers it stops passing on the strength of a spurious read.

  This one is worth noting for what it says about the phase-2 test: it asserted the right
  event and got it from the wrong signal. The `:vsplit <already-shown file>` it drives is
  the *current* file, so the `BufWinEnter` it saw came from the reload's `loaded_in_place`
  path, not from the window diff the phase existed to build.

With those in, nxvim's log is identical to nvim's on every one of the twelve commands
(`:tabnew`, `:tabnext`, `:tabclose`, `:vsplit <shown file>`, `:quit`, `:split`,
`<C-w>w`, `:e!`, `:tabnew <shown file>`, `:enew`, `:b <name>`, `<C-w>x`).

One further defect surfaced through this work but belongs to the `'endofline'` feature,
and is recorded there (`docs/plans/2026-07-26-endofline.md`, "Follow-up"): an LSP position
one row past the end of an unterminated document resolved a line short, so any
whole-document format of a file with no trailing newline corrupted the buffer.
