# Snippet-engine primitives — a bare-core seam for pure-Lua snippet engines

**Status:** proposed. Scope is the **core primitives only** — the small set of
generic, snippet-agnostic APIs that let a *pure-Lua external plugin* implement a
complete VSCode-class snippet engine (tabstops, mirrors, choices, variables,
transforms, completion-menu integration) with **no snippet semantics baked into
Rust**. The engine itself — `nxvim-snippets`, a standalone plugin capable of
loading the full [friendly-snippets] VSCode collection — is **out of scope here**
and ships in its own repo against these primitives.

This is the "approach B" from the design discussion: **own the whole snippet
session in Lua**, the way [luasnip]/[nvim-snippet] are pure Lua on neovim's
extmark + `on_bytes` + `set_text` + cursor primitives. The alternative — extend
the existing Rust tabstop session (`nx.snippet`) with variable/transform/accept
hooks — was rejected because it grows core with VSCode-specific semantics and
serves only snippets. The primitives below are each independently useful (any
plugin doing live text surgery — refactor-rename previews, structural editing,
paired-edit widgets — wants the same four), which is the bar ADR 0002 sets for a
new native seam.

`nx.snippet` (the Rust session) **stays** as the batteries-included default most
configs use. Nothing here removes or changes it; these primitives sit *beside*
it so a plugin that wants the full VSCode surface can bypass it.

---

## Design principle

> Core keeps only what a plugin *cannot* be: the pure synchronous core, the
> frame/renderer, and the native engines. Everything that can reasonably be an
> `nx.*` Lua plugin *is* one. — architecture.md guiding principle #3

A snippet engine *can* be a pure-Lua plugin, provided core exposes the generic
text-editing primitives it stands on. It cannot today, for five concrete
reasons enumerated below. Each fix is a **generic** primitive (precise range
edit, extmark gravity, an edit-notification channel, a completion-accept hook, a
cursor/mode primitive) — none mentions "snippet". That is the whole point:
core grows a capability, not a feature.

### The luasnip parallel

luasnip is ~pure Lua on exactly this primitive set:

| luasnip needs | neovim API | nxvim today |
| --- | --- | --- |
| anchor tabstop/mirror regions that shift with edits | `nvim_buf_set_extmark` + gravity | **PARTIAL** — marks exist, gravity ignored |
| read a region's current byte range | `nvim_buf_get_extmark_by_id` | **EXISTS** — `nx.buf.extmarks` |
| edit a region's contents precisely | `nvim_buf_set_text` | **GAP** |
| react to each edit (sync mirrors, detect node exit) | `nvim_buf_attach` `on_bytes` | **GAP** (channel computed, not surfaced) |
| jump between tabstops / select a placeholder | `nvim_win_set_cursor`, select-mode | **GAP** (internal only) |
| read clipboard/selection/date for variables | `getreg`, `strftime`, … | **EXISTS** — `nx.reg.get`, `os.date`, `nx.uuid`, `nx.buf.text`, `nx.mark.list` |
| appear in / expand from the completion menu | `additionalTextEdits` + snippet accept | **GAP** (no Lua-source accept hook) |

nxvim already has extmarks, region reads, and the entire variable-resolution
surface. This spec closes the remaining four: **`set_text`, gravity, an edit
channel, a completion-accept hook**, plus **one cursor/mode primitive**.

---

## What the plugin owns (out of core, for orientation)

So the seam is legible, here is everything that stays **100% in the Lua
plugin** — none of it touches core:

- **VSCode JSON loading.** Read `package.json`'s `contributes.snippets` map,
  read each `*.json`/`*.code-snippets` with async `nx.fs`, normalize `body`
  (string | string[] → joined) and `prefix` (string | string[] → fan out),
  map VSCode language ids → nxvim filetypes.
- **The snippet body parser** — `$1` / `${1:default}` / `${1|a,b|}` / mirrors /
  `$VAR` / `${VAR:default}` / `${1/re/fmt/opts}` transforms. The plugin parses
  its own bodies; core never sees VSCode snippet syntax.
- **Variable resolution.** `CURRENT_*` via `os.date`, `TM_FILENAME*` via
  `nx.buf.name`, `TM_SELECTED_TEXT`/`CLIPBOARD` via `nx.reg.get`,
  `TM_CURRENT_LINE`/`WORD` via `nx.current_line`/`nx.buf.text`, `UUID` via
  `nx.uuid`, `RANDOM` in Lua. All read-only, all already available.
- **Transforms.** Regex via `nx.regex` (the vendored vim engine), format-string
  interpretation (`${1:/upcase}`, conditionals), applied by the plugin and
  written into a mirror region with `set_text` on each edit.
- **The session state machine** — tabstop order, current stop, choice popups
  (via `nx.ui.select`/`nx.complete`), `<Tab>`/`<S-Tab>` jump logic, nested
  snippets, history/undo grouping policy.
- **Completion candidates** — a normal `nx.complete.source` offering triggers
  for the current filetype; expansion happens on accept via the hook below.

Core learns none of this. It gains only generic editing power.

---

## The primitives

### P1 — `nx.buf.set_text` (precise range edit) — **the keystone**

**Gap.** `nx.buf.set_lines` (`api.lua:411`) is line-granular and rewrites whole
lines; a mirror update or placeholder deletion is a *sub-line* range replace.
`set_text` is noted absent "until a real need" (`api.lua:12`). This is that
need, and it is the one primitive without which nothing works.

**Spec.**

```lua
-- nx.buf.set_text(buffer, start_row, start_col, end_row, end_col, replacement)
--   [alias nvim_buf_set_text]
-- Replace the 0-based, end-exclusive character range with `replacement`
-- (a list of lines). Byte-offset internally, consistent with the text model.
-- One edit through the buffer's single mutation choke point, so:
--   * extmarks in every namespace shift correctly (P2), and
--   * it folds into the open insert-undo group when one exists (a snippet
--     expansion mid-insert is one undo step), else opens its own.
-- Errors loud on an out-of-range / inverted span (E-class, not a silent clamp).
```

Semantics track `nvim_buf_set_text` exactly (columns are byte offsets within the
line; `replacement` is a list of lines; an empty replacement deletes). The core
already exposes the mutation + `normalize()` + extmark-shift pipeline that
`expand_snippet` and completion-accept use (`editor/complete.rs:361`); this wraps
it for a Lua-supplied range.

**Undo grouping.** The plugin expands by (1) deleting the trigger word and (2)
inserting the body — two `set_text` calls that must be **one** undo step. Follow
the `expand_snippet` precedent (`editor/snippet.rs:122–128`): if an insert-undo
snapshot is already open, both fold into it; otherwise the first opens the group.
No new API — `set_text` inherits the editor's existing snapshot state.

### P2 — Configurable extmark gravity

**Gap.** Core hardwires neovim's default gravity (`extmark.rs:342`: start
right-gravity, end left-gravity). `nx.buf.set_extmark` *accepts*
`right_gravity` / `end_right_gravity` but **stores and ignores** them
(`api.lua:598–599`, listed under "accepted and stored but unpainted"). The
shift math (`extmark.rs:348 shift`) uses the fixed constants.

**Why it matters.** An *active/empty* tabstop is a zero-width region
(`start == end`). Under fixed default gravity, text typed at it lands *outside*
on both edges — the exact problem the Rust session works around with a manual
`anchor` byte offset it re-derives every edit (`editor/snippet.rs:17–25`). A
Lua engine has no equivalent hook to re-anchor mid-edit, so it needs *real*
per-mark gravity: an active tabstop is set left-gravity-start /
right-gravity-end so typed text grows it from within.

**Spec.** Honor the two flags already accepted by `nx.buf.set_extmark`:

```lua
nx.buf.set_extmark(buf, ns, row, col, {
  end_row = r, end_col = c,
  right_gravity = false,      -- start edge: text at start lands INSIDE
  end_right_gravity = true,   -- end edge:   text at end   lands INSIDE
})
```

Implementation: thread the two booleans into `Extmark` and branch in
`extmark.rs::shift` (`shift_right_gravity` / `shift_left_gravity` already exist
as the two behaviors — select per-mark instead of per-edge-fixed). Default
values stay neovim's, so every existing caller is unchanged.

### P3 — `nx.buf.attach{ on_bytes }` (edit-notification channel)

**Gap.** Core already computes the neovim `on_bytes` argument tuple
(`effects.rs:201 on_bytes_edit`, drained at `effects.rs:1750–1776`), but
`nvim_buf_attach` "stays absent" from Lua (`api.lua:13`). Today a plugin can
only observe edits via the `TextChangedI` autocmd (`lifecycle.rs:394`), which
fires *that an* edit happened but not *what* changed.

**Why it matters.** After each insert-mode edit the engine must (a) find which
tabstop the cursor is in, (b) recompute every mirror/transform of that tabstop,
(c) write them with `set_text`. `TextChangedI` + re-reading regions works for
small buffers, but recomputing from a full re-scan on every keystroke is the
O(N)-per-event shape the "editor must never freeze" rule forbids on large files.
The byte delta makes mirror sync incremental.

**Spec.**

```lua
-- nx.buf.attach(buffer, { on_bytes = function(buf, tick, sr, sc, byte_off,
--                                             old_er, old_ec, old_len,
--                                             new_er, new_ec, new_len) ... end })
--   -> detach()   [alias nvim_buf_attach, on_bytes only]
-- Fires once per applied edit, on the editor thread, after convergence, with the
-- neovim on_bytes tuple. Returns a detach handle; the plugin detaches on
-- session end. `on_lines` and the other attach callbacks stay deferred (honest
-- _notimpl) until a consumer needs them — this surfaces on_bytes only.
```

This is a thin Lua-facing wrapper over machinery that already exists and is
already shaped for the batched bridge (drain effects out). Positions read from
`nx.buf.extmarks` *inside* the callback reflect the post-edit tick (the callback
runs after convergence), so the engine reads current tabstop ranges there.

### P4 — Completion-accept hook for Lua sources

**Blocker.** A plugin completion item's `insert` is applied as **literal text**
(`complete.lua:262`); the only per-item callback is `resolve` (lazy docs,
`complete.lua:275`). The native `snippets` source expands via a **core-only**
path — rows carry `source_accept`, and accept routes to
`server/src/snippet.rs:72 complete_snippet_accept` → `expand_snippet` — which a
custom Lua source cannot reach. So a Lua engine can put snippets in the menu but
**cannot expand them on accept**.

**Spec.** Add an optional `on_accept` to the `nx.complete.source` item shape:

```lua
ctx.push({
  text = "for",                 -- menu label
  on_accept = function(ctx)     -- runs at accept, on the editor thread
    -- ctx = { buf, replace = {start_row, start_col, end_row, end_col} }
    -- the range of the typed trigger the engine should replace.
    engine.expand(body, ctx.replace)   -- plugin resolves vars, drives session
  end,
})
```

When present, accepting the row runs `on_accept` **instead of** the literal
`insert` splice, handing the engine the trigger range to replace (the same
`word_start..end` core computes for the native path, `server/src/snippet.rs:90`).
This is deliberately general — it is a "run Lua at accept" seam, not a
"snippet" seam: additionalTextEdits, post-accept commands, and non-snippet
expanders all use it. Accept already tolerates a source row that core can't
splice itself (`complete.rs:331`); this extends that to a Lua callback.

**Interaction with P1.** `on_accept` runs the plugin's `engine.expand`, which
calls `set_text` (P1) to delete the trigger + insert the parsed body and
`set_extmark` (P2) to anchor the tabstops — one undo group, per P1's grouping.

### P5 — A sanctioned cursor + mode primitive

**Update (already exists).** The public **`nx.win.set_cursor(win, line, col)`**
(`api.lua`, 1-based line / 0-based byte col) is already the sanctioned caret
primitive — "the explicit-win counterpart of the (intentionally-absent)
`nvim_win_set_cursor`". The ADR 0002 decision this section anticipated was in fact
already made in the codebase (cursor placement *is* allowed, via `nx.win.set_cursor`,
distinct from the nil `nvim_*` mutation API). Verified end-to-end that it serves the
jump: an insert-mode Lua keymap that calls it repositions the caret and further typing
lands at the new spot, including a caret one past the last char (a `$0` / trailing
tabstop at line end). So **P5 needs no new core code** — only the verification and this
correction.

**Deferred (placeholder select).** nxvim has no vim-style *Select* mode (the core
`Mode::Select` is the `nx.ui.select` widget, not v_CTRL-G), so "type replaces the
placeholder" isn't a one-call primitive. A pure-Lua engine approximates it with the
already-landed primitives — place the caret at the placeholder start (P5) and clear the
default on the first keystroke via `on_bytes` (P3) + `set_text` (P1). A dedicated
select-mode primitive stays deferred until an engine proves it needs distinct behavior.

**Spec.** One narrow, sanctioned primitive (resolving the ADR exception for this
case — cursor placement is not entity mutation, it is a caret move the engine
already earns by owning the session):

```lua
-- nx.win.set_cursor(win, row, col)         -- 0-based; the caret move
-- nx.snippet.enter(range_or_pos, mode)     -- OR an engine-scoped helper:
--   pos            -> enter Insert with the caret there (empty tabstop)
--   {s_row,s_col,e_row,e_col}, "select" -> Visual-select the placeholder so the
--                                          next keystroke replaces it
```

Recommendation: expose `nx.win.set_cursor` (the clean generic form; ADR 0002's
"mutation via keystrokes" rationale is about *content*, not caret position — a
caret set has no undo/content consequence) and let the engine drive mode via the
existing surfaces (enter Insert by leaving a zero-width selection at the caret;
Visual-select a placeholder via the existing visual machinery). If leaving the
generic cursor API nil is preferred, ship the engine-scoped `nx.snippet.enter`
instead — but one of the two must exist, or the plugin cannot jump.

This is the **only primitive requiring a genuine ADR 0002 decision**; P1–P4 are
"expose existing machinery" or "generalize an existing accept path".

### P6 — Select mode (placeholder auto-select) — **[landed]**

**Update (landed).** Select mode now exists in core ([`Mode::Select`]) with the Lua
primitive **`nx.win.select_range(win, s_row, s_col, e_row, e_col[, opts])`** (0-based,
byte cols, end-exclusive) — the select-mode sibling of `nx.win.set_cursor`. The range is
highlighted like a charwise Visual selection; the next printable / `<CR>` / `<BS>`
**replaces** it (delete + enter Insert with that input). `<Esc>` (nothing typed) keeps
the default; **`opts.on_escape`** chooses where it lands so the mode serves both
consumers:

```lua
"normal"   -- (default) keep the text, drop to Normal on the selection — vim's v_CTRL-G
"insert"   -- keep the text, park the caret in Insert past it (a snippet engine wants this)
```

The default is the vim-faithful `"normal"`; the `nxvim-snippets` engine passes
`on_escape = "insert"` so a bare `<Esc>` leaves it editing the placeholder. An empty
range degrades to caret-plus-Insert at the start (the empty-tabstop path), so
`select_range` is total. Its keys route through a dedicated `Editor::handle_select`
(not the Visual command grammar), so it is deliberately *not* `is_visual()` — only its
rendered selection borrows the Visual projection (via `rendered_visual_mode`).

**Keyboard entry (vim).** Beyond the programmatic primitive, Select is reachable by
keyboard the vim way: **`gh`** (charwise) / **`gH`** (linewise) from Normal, and
**`<C-g>`** to toggle Visual ↔ Select (keeping the selection and its shape). Blockwise
Select (`g<C-h>`) is unimplemented — nxvim has no blockwise Visual either. Linewise
Select (`gH`, or `<C-g>` from Visual-Line) reports vim's `S` / `S-LINE` and replaces
whole lines like `S`/`cc`; a printable / `<CR>` / `<BS>` replaces (motions do *not*
extend a Select — they are printables, so they replace, per vim). The behaviour below is
the original proposal, kept for context.

**Gap.** A placeholder tabstop `${1:default}` should expand with its default text
**selected**, so the first keystroke *replaces* it (the VSCode / LuaSnip behavior).
nxvim has no vim-style **Select mode**: the core `Mode::Select` variant is the
`nx.ui.select` *widget*, not neovim's `v_CTRL-G` selection where a printable key
deletes the highlighted text and enters Insert. With only P5 (`nx.win.set_cursor`),
the engine can place the caret at the placeholder start but can't select the word —
so `${1:name}` expands with the caret before `name` and typing inserts *before* it
rather than replacing it.

The `nxvim-snippets` plugin ships without this and documents the limitation; it can
*approximate* select with the landed primitives (place the caret at the placeholder
start (P5) and delete the default on the first keystroke via `on_bytes` (P3) +
`set_text` (P1)), but that's a heuristic — it can't tell a replacing keystroke from
an editing one, and it fights the growing-extmark gravity. A real Select mode is the
clean answer, and the only remaining thing keeping the engine from full VSCode
placeholder UX.

**Why it's its own primitive (and low priority).** Unlike P1–P5, this isn't
"expose existing machinery" — it's a **new editor mode** in core (a genuine feature,
not a seam), and it's *generically* useful (a picker/rename widget wants
select-and-replace too), not snippet-specific. It's deferred because the engine is
fully usable without it (jump + type-after works; only the "type-over-default"
nicety is missing), so it should land only when the mode is designed on its own
merits rather than bolted on for snippets.

**Spec (sketch — a full design belongs in its own doc).**

- **Core:** a real Select mode — enter over a byte range; a printable / `<BS>` /
  `<CR>` deletes the range and enters Insert with that input; `<Esc>` → Normal;
  the shifted movement keys extend it (neovim's model). Reuse the Visual selection
  machinery for the highlight; the distinguishing behavior is "printable replaces".
- **Lua primitive:** one call to enter it over a range, e.g.
  `nx.win.select_range(win, s_row, s_col, e_row, e_col)` (0-based, byte cols) — the
  select-mode sibling of `nx.win.set_cursor`. The engine calls it (instead of
  `set_cursor`) when jumping to a **non-empty** placeholder; an empty tabstop still
  uses `set_cursor` + Insert.

**Interaction.** Composes with the rest unchanged: the placeholder's growing extmark
(P2) still bounds the region, and the replacing keystroke flows through the normal
edit path, so `on_bytes` (P3) mirror sync fires exactly as it does for typed text.

**Testing.** Black-box, per convention: expand `${1:name}`, assert the default is
selected (mode + selection range), feed a printable, assert it replaced the default
(not inserted before it) and mirrors followed; feed `<Esc>` from a fresh select and
assert the default is kept and the caret parks in Insert.

---

## Minimal set, in dependency order

1. **P1 `nx.buf.set_text`** — nothing works without precise range edits. **[landed]**
2. **P2 configurable gravity** — active tabstops can't grow correctly without it. **[landed]**
3. **P4 `on_accept`** — the completion-menu seam; independent of P1/P2, small.
   **[landed]** — a `nx.complete.source` item's `on_accept = function(item, ctx)`
   runs at accept instead of the literal splice, handed the trigger range in `ctx`;
   delegated via a `PLUGIN_ACCEPT_KEY_BASE` key range beside the snippet/LSP ones.
4. **P3 `on_bytes` attach** — mirror/transform sync; needed for transforms and
   for freeze-safe sync on large files. Can land after a first tabstops-only
   milestone (which works on P1+P2+P5 + `TextChangedI`). **[landed]** — `nx.buf.attach`
   surfaces the (already-computed) `on_bytes` / `on_reload` channels; `on_lines`,
   `on_detach`, and a `vim.api.nvim_buf_attach` alias stay deferred.
5. **P5 cursor/mode** — the one ADR decision; needed for jumps. **[landed — no new
   code]** `nx.win.set_cursor` already existed and is sanctioned; verified it serves
   the insert-mode jump. Placeholder-select stays deferred (approximated via P1+P3).
6. **P6 Select mode** — placeholder auto-select. **[landed]** A real core `Mode::Select`
   (a printable / `<CR>` / `<BS>` replaces the selection and enters Insert; `<Esc>` keeps
   the default and parks in Insert), entered via the new `nx.win.select_range` Lua
   primitive. Generic — a rename/paired-edit widget wants select-and-replace too — not
   snippet-specific.

**All six primitives (P1–P6) are landed**, and the `nxvim-snippets` plugin
is built on them — it loads VSCode collections, completes from the menu, and expands
into a live tabstop session with mirrors, variables, choices, and **transforms**. It
covers ~99.98% of the real friendly-snippets collection (the rest hit a Rust-regex
lookbehind limit and fail loud). With P6 an engine gets full VSCode placeholder UX
(type-over-default), no longer approximated.

The plugin validated the primitives incrementally:

- **Milestone 1 (P1+P2+P4+P5):** load VSCode JSON, expand from the completion
  menu, tabstops + mirrors + choices + variables, jump. **Done.**
- **Milestone 2 (+P3):** live transforms (`${1/re/fmt/opts}`) and freeze-safe
  incremental mirror sync. **Done.**
- **Milestone 3 (+P6):** type-over-default placeholder selection. **Done** — the
  engine jumps to a non-empty placeholder with `nx.win.select_range` so the first
  keystroke replaces the default.

---

## Non-goals

- **Not** extending `nx.snippet` (the Rust session) with variables/transforms.
  It stays as-is, the default engine; this spec is the *escape hatch* for a
  fuller engine, per architecture.md #3.
- **Not** a VSCode-JSON loader in core. The plugin owns the format.
- **Not** the rest of `nvim_buf_attach` (`on_lines`, `on_reload`, …) — P3
  surfaces `on_bytes` only; the rest stay honest `_notimpl` until a consumer
  needs them (the extmark-layer spec's deferral pattern).
- Select mode was **not** part of the original required set (P1–P5): the engine
  proved it works without one (P5 + type-after covers jumps), so placeholder
  auto-select was broken out as the optional **P6** above rather than folded in with
  the seams. It has since **landed** as a real core `Mode::Select` + the
  `nx.win.select_range` primitive, designed on its own merits (a generic
  select-and-replace mode), not bolted onto a seam.

## Testing

Per the project's black-box convention, each primitive is proven end to end
through the harness (no unit tests):

- **P1:** feed keys to place a cursor, `exec_lua` a `nx.buf.set_text` over a
  sub-line range, assert `nvim_buf_get_lines` + that a prior `set_extmark`
  shifted correctly, and that one `u` undoes the whole edit.
- **P2:** set a zero-width extmark left/right gravity, `set_text` inside it,
  assert the mark's range via `nx.buf.extmarks` grew (vs. the default-gravity
  mark, which does not) — a mutation test: flip the flag, watch the assert fail.
- **P3:** attach `on_bytes`, feed an insert-mode edit, assert the callback got
  the correct byte tuple; assert a large-flood insert stays fast (the freeze
  guard).
- **P4:** register a source whose item carries `on_accept`, drive the menu,
  accept, assert the callback ran with the right replace-range and the literal
  `insert` splice did **not** happen.
- **P5:** `set_cursor` then assert `nvim_win_get_cursor`; select a range and
  assert the next inserted char replaced it.

A thin integration test drives all five together: register a toy 3-tabstop
snippet-with-transform source in `exec_lua`, expand from the menu, type through
the tabstops, assert buffer + cursor at each jump — the smallest possible
stand-in for the real `nxvim-snippets` plugin, living in the harness (not an
`examples/` config, per the throwaway-example rule).

[friendly-snippets]: https://github.com/rafamadriz/friendly-snippets
[luasnip]: https://github.com/L3MON4D3/LuaSnip
[nvim-snippet]: https://github.com/nvim-mini/mini.snippets
