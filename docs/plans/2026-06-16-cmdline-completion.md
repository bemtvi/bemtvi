# Command-line completion — the float-list widget's fifth orchestration

Status: IN PROGRESS (Phase 2 complete).

## Problem

The command line (`:`) offers no completion — `editor/cmdline.rs` handles typing,
history, and registers, but there is no wildmenu / suggestion path anywhere. We want
Tab-triggered, LSP-style suggestions: press `:e`, hit `<Tab>`, and get a fuzzy list of
matching commands with a docs/params preview pane — and a command registered by a
plugin must appear in that list *the same way* a built-in does (the unified payoff).

## Design

The repo already has a **unified float-list widget** (`Menu`/`MenuView`,
`docs/specs/2026-06-14-nx-ui-float-widget.md`) with four orchestrations — picker,
`nx.ui.select`, insert-completion, and the insert-completion **docs sidebar**
(`project_complete_docs`). Command-line completion is the **fifth orchestration** on
the same widget: a `MenuKind::Cmdline` menu, placed above the command line, driven by
a bundled `nx.cmdline_complete` Lua plugin that owns the command catalog.

Engine (Rust core) vs policy (nx.* Lua):
- **Core** extracts the token being completed from `Editor::cmdline`, ranks the
  candidates (`crate::fuzzy`), renders the menu (reusing `Menu`/`MenuView`), and
  applies the accept by rewriting the command-line token. It never knows *what*
  commands exist.
- **Lua** (`prelude/cmdline_complete.lua`) owns the curated command catalog (names,
  abbrevs, synopsis, help) merged with `nx.user_command.get()`, and returns the
  candidate set for a given command line.

The insert-completion engine (`editor/complete.rs`) is **not** reused directly — it is
bound to buffer/insert semantics. We reuse its *types and patterns* (`Menu`,
`MenuView`, `fuzzy::rank`, the docs-sidebar projection), leaving it untouched.

### Decisions
- **Names + docs** only. Argument completion is out of scope; the source receives the
  full line + cursor, so it is a pure-Lua extension later.
- **On-demand**: `<Tab>` opens the menu. Open, it behaves live (typing narrows,
  `<Tab>`/`<S-Tab>` cycle, `<Esc>` closes then a second `<Esc>` cancels the line,
  `<CR>` accepts the selection then executes). `<C-n>`/`<C-p>`/`<C-e>` are already
  bound in the cmdline bucket (history/cursor), so completion overloads the existing
  `cancel`/`submit`/history actions when the menu is open rather than colliding.
- **Curated Lua catalog**, with a coverage test guarding drift.

### Synchronous, not streamed
The catalog filter is a microsecond table scan, so there is no async / generation
machinery (that exists for slow insert sources — rg / lsp). `<Tab>` (and each edit
while the menu is open) sets `Editor::cmdline_complete_request`; the server resolves it
in one Lua round-trip (`nx._cmdline_complete_run(line, col)` → candidates) and rebuilds
the menu via `Editor::open_cmdline_menu`.

## Phases (commit + pause between each)

- **Phase 1** — Core engine (`editor/cmdcomplete.rs`, `MenuKind::Cmdline`,
  `MenuPlacement::Cmdline`), the request/setup server seam, redraw projection above the
  command line, bundled `prelude/cmdline_complete.lua` catalog + `<Tab>` map.
  `<Tab>` opens a ranked name menu; typing narrows; `<Esc>` closes.
- **Phase 2** ✅ — Navigation + accept + execute. `<Tab>`/`<S-Tab>` (and the
  overloaded `<C-n>`/`<C-p>`/`<Down>`/`<Up>` history keys) cycle the wildmenu
  selection; it opens noselect, so the first nav highlights row 0 (`<Tab>`) or the
  last (`<S-Tab>`). `<CR>` (submit) accepts the highlighted row — rewriting the
  command-name token `[anchor..col)` in place — then executes; a noselect popup runs
  the typed line unchanged. The selection logic is shared with the insert-completion
  popup (`Menu::select_next`/`select_prev`).
- **Phase 3** — Docs preview sidebar (synopsis + help), reusing `project_complete_docs`.
- **Phase 4** — Unified plugin commands (`nx.user_command.create` gains `desc`),
  catalog coverage test, `examples/cmdline-completion/`, docs.

## Key files
- Core: `editor/cmdcomplete.rs` (new), `editor/menu.rs`, `editor/cmdline.rs`,
  `editor/mod.rs`, `lib.rs`, `view.rs`.
- Server: `runtime.rs`, `install.rs`, `effects.rs`, `redraw.rs`.
- Lua: `prelude/cmdline_complete.lua` (new, registered in `PRELUDE_MODULES`),
  `prelude/keymap.lua`.
- Tests: `crates/nxvim-server/tests/cmdline_complete.rs` (new).

## Verification
Black-box harness tests on the redraw `menu` map (names listed, narrowing, accept,
execute, docs, plugin-command appearance, catalog coverage); `cargo test --workspace`;
`cargo fmt` + `clippy -D warnings`; manual `examples/cmdline-completion/`.
