# `/` search — design & phased plan

**Date:** 2026-06-02
**Status:** Phases 1–5 implemented

## Goal

Add vim's **search** to nxvim: the `/` and `?` command-line searches, `n`/`N`
repeat, match highlighting (`hlsearch`/`incsearch`), the search options
(`ignorecase`/`smartcase`/`wrapscan`), and real pattern matching. This is the
last big editing primitive on the roadmap's short list (architecture.md →
*Not yet implemented*: "search (`/`, `?`, `:s`)"). `:s` substitution and the
quickfix-style `:vimgrep` are **out of scope** here — this feature is the
interactive *cursor* search only.

The work is split into five phases, each of which **builds, passes
`cargo test --workspace`, and is independently shippable**, so each can be handed
to a fresh context window once the previous has landed. Phases 1–4 are the
feature; Phase 5 is optional polish.

---

## Where it slots into the architecture

Search is a **core** (`nxvim-core`) concern: the core owns the buffer text, the
cursor, the modes, and the command line, so it owns *finding* the match and
*moving* the cursor. The server and TUI only gain a little plumbing:

- **`nxvim-core`** — a search command-line (reusing `Mode::Command`), the match
  engine, `last_search` state for `n`/`N`, the options, and the match spans
  projected into the `View` (the way `selection` already is).
- **`nxvim-server`** — projects the new `View` fields into the `redraw` map and
  resolves the `Search`/`IncSearch` highlight groups to styles (the same path
  `chrome_styles` uses today). `:nohlsearch` already exists as a no-op stub
  (`editor.rs`, the `"noh" | "nohlsearch"` arm) and gets wired up.
- **`nxvim-tui`** — renders the `/`/`?` prompt char (today the command line is
  hard-coded to `:`), and paints the search-match spans.

Key existing seams we lean on:

- **Command-line mode already exists.** `enter_command` / `handle_command`
  (`editor.rs`) drive `:` ex input through `Mode::Command`. Search is the same
  mode with a different *kind* and a different action on `<CR>`.
- **`selection` is the template for match highlighting.** `View::selection`
  (`view.rs`) is a per-row `Option<(start_col, end_col)>` of screen columns the
  core computes (tab/wide-char aware via `unicode::virtcol`). Search matches are
  the same shape — a `Vec<(start,end)>` per row — and ride the same projection.
- **Highlight groups resolve on the server.** `chrome_styles` resolves named
  groups (`Visual`, `StatusLine`, …) to palette ids; `Search` and `IncSearch`
  join that list. catppuccin already defines both via `nvim_set_hl`, so they
  light up for free once resolved.
- **Black-box tests.** All coverage goes in `crates/nxvim-server/tests/editing.rs`
  (cursor/lines/redraw assertions) and the screen tiers
  (`crates/nxvim/tests/screen.rs`, `crates/nxvim-tui/tests/`) for the painted
  highlight — no unit tests (architecture.md → *Testing philosophy*).

---

## Phase 1 — search mode, literal engine, `n`/`N`  *(done)*

**Outcome:** `/foo<CR>` jumps the cursor to the next literal occurrence of
`foo`; `?foo<CR>` searches backward; `n` repeats in the same direction and `N`
in the opposite; `<Esc>` cancels; an empty pattern repeats the last one; misses
report `E486`; searches wrap (`wrapscan` behavior, on) with the
"search hit BOTTOM/TOP" notice. **Literal substring matching only** — regex
lands in Phase 4. **No match highlighting yet** — that is Phase 3.

### Core (`editor.rs`)

- A `SearchDir { Forward, Backward }` enum (with `opposite()` and a `prefix()`
  returning `'/'` / `'?'`).
- A `CmdlineKind { Ex, Search(SearchDir) }` field on `Editor`, set on entry and
  read on `<CR>`, so one `Mode::Command` serves both `:` and `/`,`?`.
- `enter_command` sets `CmdlineKind::Ex`; a new `enter_search(dir)` sets
  `CmdlineKind::Search(dir)`. `/` and `?` in `handle_normal_command` call it.
- `handle_command`'s `<CR>` dispatches on the kind: `Ex → execute_ex`,
  `Search(dir) → submit_search(text, dir)`.
- `last_search: Option<(String, SearchDir)>` on `Editor` for repeat / empty
  pattern. `submit_search` records it (empty pattern reuses the stored one, or
  `E35: No previous regular expression`).
- `do_search(pattern, dir)` — the literal matcher. Works on a `String` snapshot
  of the rope; forward starts one grapheme past the cursor (`next_grapheme_idx`,
  boundary-safe) and `str::find`s, wrapping to `find` from the top; backward
  `rfind`s in `text[..cursor]`, wrapping to a whole-buffer `rfind`. Moves the
  cursor with `set_cursor_char` + `clamp_cursor`. On a wrap, echoes the
  BOTTOM/TOP notice; otherwise sets `message = "{prefix}{pattern}"`. A miss
  echoes `E486: Pattern not found: {pattern}`.
- `n` / `N` in `handle_normal_command` call a `search_repeat(same, count)` that
  loops `do_search` `count` times over `last_search` (or `E35`).

### View / server / TUI

- `View::cmdline_prefix: char` (`':'`/`'/'`/`'?'`), from a
  `Editor::cmdline_prefix()`. Server adds a `cmdline_prefix` string to the
  `redraw` map; the TUI mirrors it and `render_command` uses it instead of the
  literal `:`.

### Tests (`editing.rs`)

`/`-forward + wrap, `?`-backward, `n`/`N` (incl. opposite-direction and a
count), empty-pattern repeat, `E486` miss, `<Esc>` cancel leaves the cursor put,
and a `redraw`-level check that the command line shows `/` while typing.

### Known gaps after Phase 1 (closed by later phases)

Case is always sensitive (Phase 2); no `ignorecase`/`smartcase`/`wrapscan`
toggles (Phase 2); no highlighting (Phase 3); literal only — `.`, `*`, `^`, `$`
are matched verbatim (Phase 4); `/` from visual mode drops the selection, and
`d/pat` is not yet an operator motion (Phase 5).

---

## Phase 2 — options, messages, history  *(done)*

**Outcome:** search respects `ignorecase`, `smartcase`, and `wrapscan`; a
count-prefixed `3/foo<CR>` / `3n` finds the Nth match; `nowrapscan` reports
`E384`/`E385` (hit BOTTOM/TOP without moving) instead of wrapping; search
history is recallable with `<Up>`/`<Down>` (and `<C-p>`/`<C-n>`) in the search
command line.

- Extend `options.rs` + `Options` with `ignorecase`, `smartcase`, `hlsearch`,
  `incsearch`, `wrapscan` (booleans; `wrapscan`/`incsearch`/`hlsearch` default
  on as in modern nvim). Wire them through `ex_set` / `apply_set` (today only
  `number`/`relativenumber` are honored).
- `do_search` lowercases both sides when `ignorecase`, with `smartcase`
  suppressing it for a pattern containing an uppercase char. (`\c`/`\C` in the
  pattern override this — but those are regex atoms, so the override itself can
  wait for Phase 4; the option behavior lands here.)
- `nowrapscan`: a miss past the end is `E385: search hit BOTTOM without match
  for: {pat}` (forward) / `E384` (backward), cursor unmoved.
- A `search_history: Vec<String>` on `Editor`; the search command line walks it.
  This needs `handle_command` to grow `<Up>`/`<Down>` handling for the search
  kind (the ex kind can share it later).

### Tests

ignorecase on/off, smartcase mixed-case, count search, `nowrapscan` E385/E384,
history recall.

---

## Phase 3 — `hlsearch` + `incsearch`  *(done)*

**Outcome:** all matches of the active search are highlighted (`hlsearch`); the
match under construction is previewed live while typing (`incsearch`), jumping
the viewport to it; `:noh` clears the highlight until the next search.

As shipped:

- **Match spans in the `View`.** Two layers, both computed for the visible window
  only with the same `virtcol` screen-column conversion `selection` uses:
  `View::search: Vec<Vec<(usize,usize)>>` (every match per row, the `Search`
  group) and `View::incsearch: Vec<Option<(usize,usize)>>` (the one match the live
  preview rests on, the `IncSearch` group). Both come from one
  `Editor::search_highlights` pass. Gated on `hlsearch && search_active`, where
  `search_active` is set by a committed search and cleared by `:noh` (the existing
  stub is now wired). It **persists across edits** — vim keeps `hlsearch` lit
  until `:noh` or a new search, so the earlier "clear on editing" note was dropped
  as un-vimlike (the spans recompute from the pattern each frame regardless).
- **Incsearch.** `search_origin: Cursor` is saved in `enter_search`. Every
  cmdline-editing keystroke in the search prompt re-runs a *provisional*
  `preview_match` (a side-effect-free sibling of `do_search` — no message,
  history, or `last_search` change) from that fixed origin and hops the cursor
  there; `ensure_visible` scrolls the preview into view. `<Esc>` (and an empty
  `<Backspace>`) rewind the cursor to the origin; the committed `<CR>` also rewinds
  first, then runs the real search from the origin so the count search is
  deterministic and identical to the no-incsearch path.
- **Server/TUI.** `search` (a per-row array-of-pairs via `multi_spans_value`) and
  `incsearch` (reusing `spans_value`) join the `redraw` map; `Search`/`IncSearch`
  resolve through the same `chrome_styles` path as `Visual`. The TUI paints them
  on top of the syntax tokens and under the visual selection, falling back to a
  built-in yellow highlight when no colorscheme defines the groups.

### Tests

`editing.rs`: `hlsearch` projects a match span per line, `:noh` clears them all,
`incsearch` previews the next match while typing, `<Esc>` restores the origin.
`screen.rs` (tier 2): the painted cells of both matches carry the highlight and
`:noh` clears them.

---

## Phase 4 — regex patterns (canonical / "perl-compatible")  *(done)*

**Outcome:** patterns are real regexes, not literals — and, by a **deliberate
divergence from vim**, they are *canonical* (Perl/PCRE/RE2-style) regexes, not
vim's "magic" dialect. `+ ? * ( ) | { } [ ] ^ $ .` are operators by default and a
leading `\` escapes them to a literal (the inverse of vim, where `\+` is the
operator and bare `+` is literal). Per-pattern case is the standard inline
`(?i)`/`(?-i)` flag rather than vim's `\c`/`\C`.

> **Divergence note.** This is the one place nxvim intentionally does *not* clone
> vim. The Rust `regex` engine's syntax is exposed directly because it is the
> familiar everyday regex flavor; users coming from vim must write `\+` for a
> literal plus and `(?i)` instead of `\c`. Recorded here so it isn't mistaken for
> an oversight.

As shipped:

- **`search.rs` module** in `nxvim-core` (pure/sync — `regex` is a pure
  dependency, no I/O): a thin `SearchRegex` wrapper that compiles the pattern
  *directly* with `RegexBuilder` (no vim→regex translation layer). The `regex`
  crate is exact-pinned (`=1.12.3`) in `[workspace.dependencies]`. It is not full
  PCRE — no backreferences or look-around — but covers the everyday surface.
- **Line-by-line scanning.** `search_matches` compiles once (`compile_search`)
  and walks lines via `match_forward_from` / `match_backward_before` (each its
  own haystack, so `^`/`$` anchor to line edges and the trailing-`\n` invariant
  never bites). `search_highlights` uses the same compiled regex's `find_all` per
  visible row. **Multi-line patterns (`\n` in the pattern) are not supported.**
- **Case.** `RegexBuilder::case_insensitive` (full Unicode folding, replacing the
  Phase 2 ASCII-only fold) seeded from the `ignorecase`/`smartcase` option; an
  inline `(?i)`/`(?-i)` in the pattern overrides it.
- **Errors.** A pattern the engine rejects reports `E383: Invalid search
  string: {pat}` on the message line, cursor unmoved (incsearch/hlsearch just
  show nothing while a half-typed pattern doesn't compile).

### Tests

`editing.rs`: dot wildcard, escaped-literal dot, `^`/`$` anchors, a `[0-9]`
class, a `+` quantifier, bare-`+`-operator vs escaped-`\+`-literal, `|`
alternation, `\b` whole-word boundary, `(?i)`/`(?-i)` case flags, and an `E383`
bad-pattern miss.

---

## Phase 5 — `*`/`#`, operator motion, offsets  *(done)*

**Outcome:** `*`/`#` (and `g*`/`g#`) search the word under the cursor;
`d/pat<CR>`, `y/pat<CR>`, `c/pat<CR>` use search as an operator motion;
search offsets (`/pat/e`, `/pat/+2`) place the cursor relative to the match.

As shipped:

- **`*`/`#`.** `search_word_under_cursor` grabs the keyword under (or next on the
  line after) the cursor and runs a search — forward for `*`, backward for `#`.
  The plain forms wrap it in `\b…\b` (whole-word); `g*`/`g#` use the bare word
  (substring). Note the boundary is canonical-regex `\b`, not vim's `\<…\>`, per
  the Phase 4 divergence. The pattern is recorded in `last_search`, so `n`/`N`
  repeat it; `E348` when there is no word under the cursor.
- **Offsets.** `split_search_offset` peels a trailing offset off the submitted
  line at its **last unescaped separator** (`/` for forward, `?` for backward —
  escape it as `\/` to search a literal one). `SearchOffset` covers `e[±n]` (match
  end), `s`/`b`[±n] (match start), and a bare `[+-]n` line offset; it is stored
  with `last_search` so `n`/`N` and incsearch reuse it, and `place_with_offset`
  applies it (a bare `+`/`-` means ±1, as in vim).
- **Operator motion.** A pending operator + `/`,`?` stashes the operator
  (`search_operator`) and opens the search prompt; the committed `<CR>` runs the
  search from the origin and hands `apply_operator` a `MotionResult` whose
  inclusiveness comes from the offset (exclusive to the match start, inclusive
  for `/e`, linewise for a line offset). `<Esc>` abandons both the search and the
  operator. The search loop now walks a *local* cursor so a miss leaves the real
  cursor (and buffer) untouched — `do_search` became `run_search`.

### Tests

`editing.rs`: `*`/`#` whole-word search forward/backward, `g*` substring,
whole-word rejection of a superstring, `d/` (exclusive) and `c/`, `<Esc>`
aborting the operator, `/pat/e` cursor placement, `/pat/e` making `d` inclusive,
and a `/pat/+1` line offset.

---

## Risks & decisions

- **Regex dialect.** Vim's regex is its own language. Rather than translate it,
  Phase 4 deliberately adopts the *canonical* (Perl/PCRE/RE2) flavor the Rust
  `regex` crate already speaks — a divergence from vim, documented in Phase 4
  above. We still shipped a *literal* engine first (Phase 1) so the whole
  mode/UX/highlight stack was proven before the dialect work.
- **Multi-line matches deferred.** Line-by-line scanning keeps highlighting and
  the trailing-newline invariant simple; `\n`-spanning patterns are a noted
  follow-up.
- **One window today.** Search moves the single window's cursor; nothing here
  conflicts with the future window work.
- **`:s` and `:vimgrep` are separate features**, not part of this plan.
