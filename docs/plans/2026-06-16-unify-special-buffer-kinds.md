# Unify the special-buffer-kind grab-bag

Status: **planned** — 2026-06-16. A refactor to consolidate nxvim's accreted
"non-ordinary buffer" mechanisms before more is built on them (the `nx.view` work
exposed how out of hand this has gotten). No new user-facing feature — this is a
consistency / correctness cleanup that also closes a real read-only hole.

## Problem

nxvim has grown **six** kinds of non-ordinary buffer, each marked, made read-only,
routed, and rendered a *different* way. Adding `nx.view` made it seven mechanisms in
five styles — a seventh parallel copy, not a generalization.

| kind | marked by | read-only via | input routing | render |
|---|---|---|---|---|
| explorer | `Buffer.dir: Option<PathBuf>` (`buffer.rs:166`) | input-routing **only** (NOT `modifiable()`) | `KeyContext::Explorer` → `'E'` bucket | ordinary `WindowView` |
| view (`nx.view`) | `Buffer.view: Option<u64>` (`buffer.rs:175`) | `modifiable()` **+** input-routing | `KeyContext::View` → `'W'` bucket | ordinary `WindowView` |
| terminal | `Buffer.terminal: bool` (`buffer.rs:184`) | `modifiable()` + `Mode::Terminal` | `Mode::Terminal` (forwards to PTY) | ordinary `WindowView` |
| image | `Buffer.image: bool` (`buffer.rs:197`) | empty rope (nothing to edit) | none | `WindowView.image: ImageView` |
| quickfix / loclist | `Editor.qf_bufnr` + per-window `Window.loclist_bufnr` registry | `modifiable()` + special-cased `<CR>` in `input()` | none (`KeyContext::Editing`); `<CR>` hard-coded | ordinary `WindowView` |
| panel (`:messages`/`:ls`/loclist) | `Editor.panel: Option<Panel>` overlay struct | overlay grabs all keys | `KeyContext::Panel` → `'L'` bucket | separate `PanelView` chrome |

Three concrete problems fall out of this:

1. **Read-only enforcement is split and incomplete.** `Editor::modifiable()`
   (`terminal.rs:143`, now `!terminal && !is_quickfix_buffer() && !is_view_buffer()`)
   is consulted at the edit chokepoints, but the **explorer** isn't in it — its
   inertness rides input-routing only, so an ex-command (`:d`, `:s`, `:put`) edits a
   netrw listing despite its doc claiming it "can't be corrupted." (The `nx.view`
   work already hit this and added the three ex-command guards in `ex.rs`; the
   explorer is the same latent bug, unfixed.) Two mechanisms enforcing one property,
   neither completely.
2. **Identity is scattered across three owners.** Four `Buffer` fields
   (`dir`/`terminal`/`image`/`view`), one `Editor` field (`qf_bufnr`), one `Window`
   field (`loclist_bufnr`), one overlay (`panel`). There is no single "what kind is
   this buffer?" — `buffer_buftype()` (`quickfix.rs:425`) already reaches across two
   of them by hand and is the only place that even tries.
3. **Input routing is six special cases.** `key_context()` (`menu.rs:944`) has a
   branch per kind, `input()` (`mod.rs` ~1510–1595) has a branch per kind, and
   `KeyContext` (`mode.rs:36`) has a variant per kind — explorer and view are
   near-identical copies (inert + `:/?` fallthrough + a bucket + an `apply_*_action`).

## The target: the quickfix model (don't reinvent it)

The quickfix window already solves this *correctly*, and it's vim's well-understood
model — so the goal is to **converge the bespoke kinds onto it**, not to invent a new
abstraction (`BufferKind` enum, a unified routing skeleton, a new `KeyContext`). The
quickfix branch's own comment in `input()` (`mod.rs:1560`) is the spec:

> The quickfix window is an ordinary window onto a `nomodifiable` buffer (vim's
> model): every normal-mode key — motions, search, `<C-w>…`, `:` — flows through
> unchanged, and edits are refused with `E21` at the `modifiable()` chokepoints. The
> one special key is `<CR>`, which jumps to the entry on the cursor's line.

That is the entire pattern, and it has **four** ingredients, all already present:

1. **Read-only via `modifiable()` at the edit chokepoints** — not via input-routing.
2. **Ordinary `WindowView` rendering** — no special projection.
3. **Normal-mode keys flow through unchanged** — `j`/`k`/`gg`/`G`/`/`/`:`/`<C-w>` are
   just normal motions on a normal buffer. No per-kind nav actions exist or are needed.
4. **Only the activation key is special** (`<CR>` → jump), special-cased in `input()`.

Compare what explorer and view do *instead* (the reinvention to delete): an early
`return` in `input()` (`mod.rs:1529` / `1555`) that swallows every key via
`handle_explorer_text` / `handle_view_text`, a dedicated `KeyContext` variant
(`mode.rs:48` / `53`), a dedicated keymap bucket (`'E'` / `'W'`), and a full
`apply_explorer_action` / `apply_view_action` re-implementing `next`/`prev`/`first`/
`last`/`half`/`page` — **motions that the normal-mode grammar already provides on any
buffer.** The whole apparatus exists to re-do what a `nomodifiable` ordinary buffer
gets for free.

End state: explorer, view, **and quickfix itself** are `nomodifiable` ordinary
buffers distinguished only by their identity marker, their content source, and their
one or two special keys — and even quickfix's `<CR>` stops being an editor hard-code.
**`input()` ends with zero special-buffer branches.** In vim, quickfix's `<CR>` is a
buffer-local mapping (`nnoremap <buffer> <CR> …`) installed by its ftplugin — *that*
is the model, not a branch in the input loop. nxvim's current hard-coded quickfix
`<CR>` (`mod.rs:1565`) is itself a bespoke hard-code to remove.

## Phase 1 — one read-only mechanism, consulted everywhere (the quickfix way; small)

This *is* ingredient 1, applied uniformly. No new abstraction.

- Add `Buffer::read_only(&self) -> bool` from the existing markers
  (`dir.is_some() || view.is_some() || terminal || image`), and rewrite
  `Editor::modifiable()` as `!self.buffer().read_only() && !self.is_quickfix_buffer()`.
  This folds the **explorer** (and image) into the same chokepoint enforcement quickfix
  already uses — closing the demonstrated `:d`-corrupts-a-listing hole.
- **Audit every edit chokepoint** consults `modifiable()` (inventory: `insert.rs:18`,
  `operators.rs:55/711/796`, `ex.rs` s/d/put, `multicursor.rs:524`, `snippet.rs:113`,
  the Replace/normal paths) — confirm each refuses with `refuse_edit()` (E21).
- Tests: `:d`/`:s`/`:put`/`dd`/`p`/`R`/`i` refused with E21 on explorer, view, quickfix,
  terminal, image. (Inventory found **no** existing such tests — this is the regression
  net for Phase 2.)

Ships independently; fixes the bug. After this, the explorer/view input-routing
inertness is **redundant** — which sets up Phase 2.

## Phase 2 — converge explorer + view + quickfix on buffer-local maps (the big deletion)

With read-only enforced at the chokepoints (Phase 1), the bespoke routing has nothing
left to do. Delete every special-buffer branch from `input()` and let normal keys flow;
carry the activation keys as **buffer-local default keymaps** — vim's ftplugin model —
so quickfix, explorer, and view all use one identical mechanism and `input()` keeps
**no** special-buffer branch.

**Delete from `input()` (`mod.rs`):**
- the explorer early-return (`1529`) and view early-return (`1555`) branches, and
- the **quickfix `<CR>` hard-code** (`1565`). All three go.

After this, normal-mode keys flow through the grammar on all three: `j`/`k`/motions/
search/`:`/`<C-w>` are ordinary motion on a `nomodifiable` buffer (edits refused by
Phase 1).

**Activation keys become buffer-local default maps** installed the vim way — by a
prelude `FileType` autocmd, keyed off each kind's filetype/buftype:
- quickfix/loclist (`buftype=quickfix`): `<CR>` → jump to entry.
- explorer (give the listing a filetype, e.g. `netrw`/`nxdir`): `<CR>` → open, `-` → parent.
- view (its `nx.view.create{ filetype = … }`, falling back to a default `nxview` ft):
  `<CR>` → fire `on_select`.

Each map's RHS is a thin Lua function calling the **existing native action bridge**
(`nx._explorer_action("open")`, `nx._view_action("confirm")`, and a new
`nx._qf_action("jump")` for quickfix — the only new bridge), so the core jump/open/
select logic is unchanged; only the *trigger* moves from a hard-coded branch to a
buffer-local map. `default = true` keeps them user-overridable via the standard
`nx.keymap.set(mode, lhs, rhs, { buffer = … })` — no bespoke bucket.

**Delete the now-dead apparatus:**
- `KeyContext::Explorer` / `KeyContext::View` (`mode.rs`) and their two branches in
  `key_context()` (`menu.rs:944`);
- the `'E'` / `'W'` buckets and their entries in `widget_bucket` / `mode_buckets` /
  `mode_code` (`nxvim-server/src/keymap.rs`);
- `handle_explorer_text` / `handle_view_text`;
- the **nav** arms of `apply_explorer_action` / `apply_view_action`
  (`next`/`prev`/`first`/`last`/`half`/`page` — normal motions already do these); keep
  only `open`/`up` (explorer) and `confirm` (view), now invoked by the buffer-local maps.

**Lifecycle — feasibility confirmed; one wiring task remains.** The two primitives the
mechanism needs already exist:
- **Buffer-local default maps are first-class.** The keymap snapshot sorts
  `(buffer.is_some(), !default, seq)` (`keymap.rs:444`) — precedence is **buffer-local >
  global**, and within a scope **user-override > default**, with per-buffer tries built
  by `build_for(buffer)`. So a `default = true` buffer-local `<CR>` map is exactly
  overridable the standard way.
- **`FileType` lifecycle machinery exists** (`lifecycle.rs`, `lib.rs:1135/1842`:
  `BufReadPost → FileType → BufEnter`, gated by an `announced` set).

The remaining task: the explorer and quickfix display buffers **don't carry a filetype
today** (only the view does, via `set_filetype` in `create_view`). So Phase 2 must give
the explorer listing a filetype (e.g. `nxdir`) and the qf display buffer `qf`, and
ensure `FileType` fires for these core-created buffers (it's keyed off naming/announce —
verify the dir-named explorer buffer and the qf buffer reach the announce path; make
them if not). This is correct vim behavior regardless (vim's quickfix is `filetype=qf`,
netrw is `filetype=netrw`). Fallback if an event proves awkward for one kind: install
its buffer-local default maps directly at creation. **Settle this wiring first in
Phase 2, but it is a wiring detail, not a feasibility risk.**

**Scope boundary:** the grabbing overlays (picker / select / panel) genuinely grab all
input and **keep their buckets** (`'P'`/`'S'`/`'L'`) — they are not buffers-in-windows.
Only the three buffer kinds (explorer, view, quickfix) move. The
[configurable widget keys](2026-06-16-configurable-widget-keys.md) note is updated:
explorer/view rebinding moves from the `'E'`/`'W'` bucket to buffer-local maps (and
quickfix `<CR>` becomes rebindable for the first time).

Net: three `input()` branches, two `KeyContext` variants, two buckets, two
`handle_*_text`, and the nav action sets all **deleted**; explorer/view/quickfix become
one shape — a `nomodifiable` buffer with buffer-local activation maps.

## Identity: keep the markers; no enum unless it earns its keep

Deliberately **not** introducing a `BufferKind` enum as part of this — that would be
reinventing identity storage the user explicitly warned against. The existing markers
(`Buffer.dir`/`view`/`terminal`/`image` + the quickfix registry) stay; `read_only()`
and `buffer_buftype()` read them. If, after Phases 1–2, the handful of `match`-able
call sites makes an enum clearly pay for itself, fold the four `Buffer` fields into one
then — as a trailing, optional, purely-mechanical tidy, not a goal.

## Later / separate — the bottom panel

The panel is a grabbing overlay with its own `PanelView` chrome, word-wrap
(`panel.rs:16`), and title strip — not a buffer-in-a-window. The quickfix model *does*
extend to it (a `nomodifiable` buffer in the bottom dock, `<CR>` special-cased,
selection via the existing `jump_to` / `on_select`), retiring the whole `Panel` /
`PanelView` / `'L'`-bucket stack. But it carries real UX-parity work (wrap, the
open-on-last-line semantics, the hidden-dock chip rendering in `redraw.rs:267`), so it
is **its own plan** — `docs/plans/<date>-panel-as-view.md` — not a tail of this one.

## Out of scope

- The terminal **mode** (`Mode::Terminal`) + PTY forwarding — terminal keeps its mode;
  only its read-only-ness joins the unified `modifiable()` (already true).
- The float widgets (picker / select / content-float) — transient grabbing overlays,
  a different axis; they keep their buckets.
- Any change to the public `nx.view` Lua surface — internal consolidation only.

## Sequencing & risk

- **Phase 1 first** — tiny, ships the bug fix, and builds the read-only test net.
- **Phase 2** — gated behind Phase 1's tests; it's a *deletion* (less code, fewer
  concepts), with the activation-key (a)/(b) decision the only design choice.
- The panel is a separate effort; record the direction, don't bundle it.

Each phase keeps the whole workspace green (`cargo test --workspace`, `clippy -D
warnings`, both default and `--no-default-features`) and is independently mergeable.
