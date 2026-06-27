# Cmdline file-path completion via the fuzzy picker

Status: DONE (2026-06-26) — all four phases landed; `cmdline_complete.rs` 37 tests
green (incl. the file-picker e2e + example-loads guard), `picker.rs` 56 green,
full `nxvim-server` suite green, native + `--no-default-features` build, clippy
clean. (Pre-existing unrelated failure: `nxvim` `code_action_lists_then_applies…`
fails on a clean tree too.)

## The gap

The command-line completion engine (`nx.cmdline_complete`,
`docs/plans/2026-06-16-cmdline-completion.md`) completes command **names** and
`:set` **option** names, but not file paths: `:e <Tab>` opens nothing (the
`non_set_command_args_open_no_menu` test pins that). This adds file-path
completion for the file-taking commands.

## Decision: a picker handoff, not a wildmenu

Two product decisions (asked of the user):

- **Surface:** the full `nx.picker` overlay (centered float, file preview pane),
  not the inline wildmenu. So `<Tab>` on a file argument *hands off* from the
  command line to the picker.
- **Matching:** fuzzy, but **prioritize same-level candidates** — the entries of
  the directory the typed prefix points at rank first, before anything deeper.

Why a handoff (vs. extending the inline wildmenu): the wildmenu source is
resolved **synchronously** (one round-trip in `effects.rs`), but file listing is
**async-only** (`nx.fs`, the no-blocking-IO principle; sync `read_dir` would also
break over the daemon/wasm). The picker is already an async, streaming,
fuzzy-matched overlay — exactly the right engine. The cmdline just becomes a
launcher into it.

## Flow

```
:e src/ed<Tab>
   │  core: arg region of a file-taking command
   ▼
nx._cmdline_complete_run(line, col)        (Lua policy owns "which commands take files")
   │  stores ctx { line_prefix = line[:anchor], dirs_only }
   │  nx.picker.open("cmdline_files", { query = "src/ed" })   ← queued
   │  returns the PICKER-LAUNCHED sentinel
   ▼
server: editor.cancel_cmdline()  → Normal mode (the ":" line is dismissed)
        apply_lua_effects()      → flushes the queued picker open
   ▼
picker overlay, prompt pre-filled "src/ed", same-level entries first
   │  confirm(item)
   ▼
nx.cmd(line_prefix .. item.path)           e.g. nx.cmd("e src/editor/mod.rs")
```

`line_prefix` is everything left of the argument token (`line:sub(1, anchor)`),
so the command verb **and any modifiers** (`:vertical split `, `:tab edit `) are
preserved — confirm works for every file-taking command without per-command code.

## Phases

### Phase 1 — picker initial-query seeding (Rust + Lua) ✅

KEY FIX during impl: a non-empty seed must open in *filtered* mode only for a
STATIC source; a DYNAMIC source bypasses the matcher (it filters itself from
`ctx.query`), so it stays in passthrough or its own rows get re-ranked away —
`open_picker`: `filtered = (!query.is_empty() && !dynamic).then(Vec::new)`.

Make a picker open with a pre-filled prompt that filters the initial run.
Independently useful (`nx.picker.open(name, { query = … })`).

- `ops.rs` `PickerOpenReq`: add `query: String`.
- `install.rs` `nx._picker_open`: add a trailing `query` arg → `PickerOpenReq`.
- `menu.rs` `open_picker(...)`: accept an initial prompt string; seed
  `Prompt { text, cursor=end }` instead of `Prompt::default()`.
- Server picker-open drain + initial run: kick gen-0 with the seed query, not `""`.
- `picker.lua` `nx.picker.open(name, opts)`: pass `opts.query` (default `""`)
  through `nx._picker_open`; document it.
- Test: open `files` with `{ query = "…" }`, assert the list is pre-filtered.

### Phase 2 — cmdline→picker handoff (server + Lua) ⬜

- `runtime.rs run_cmdline_complete`: return an enum — `Candidates(Vec<…>)` or
  `PickerLaunched` (the Lua source returned the sentinel). Parse a sentinel table
  (`{ __picker = true }`) vs. a candidate array.
- `effects.rs`: on `PickerLaunched` → `self.editor.cancel_cmdline()` then
  `self.apply_lua_effects()` (flushes the queued picker open). On `Candidates`,
  the existing `open_cmdline_menu` path is unchanged.
- `cancel_cmdline` is already `pub(crate)`; expose a server-reachable call if
  needed (a thin `pub fn`).
- `cmdline_complete.lua`: a `FILE_COMMANDS` policy table (file args) + a
  `DIR_COMMANDS` table (`cd`/`lcd`/`tcd` → dirs only). In `_cmdline_complete_run`,
  when the arg region belongs to one, store
  `M._pending = { line_prefix = line:sub(1, <anchor bytes>), dirs_only = … }`,
  open the picker, and return the sentinel.

### Phase 3 — the same-level file source (Lua) ⬜

`nx.picker.source{ name = "cmdline_files", dynamic = true, preview = "file", … }`:

- Read `ctx.query`; split at the last `/` into `(dir, leaf)`. Resolve `dir`
  against `ctx.cwd`, handling `~` and absolute paths.
- `nx.fs.readdir(dir)` (async). Rank entries against `leaf`: exact-prefix tier
  before subsequence tier; **directories before files** within a tier;
  directories shown with a trailing `/`. Push them — these are the **same-level**
  candidates, first.
- `item.path` is the path to splice (relative when the prefix was relative,
  re-rooted on each `/`). `confirm(item, …)`: `nx.cmd(M._pending.line_prefix ..
  item.path)`; if `item` is a directory, instead re-open the picker one level
  deeper (descend) rather than execute.
- `dirs_only` variant filters `readdir` to directories (for `:cd`).
- (Follow-up, optional) a bounded recursive-fuzzy tail appended after the
  same-level entries, for cross-directory fuzzy jumps. Out of scope for v1.

### Phase 4 — tests, example, docs ⬜

- Rewrite `non_set_command_args_open_no_menu` → `file_command_arg_opens_picker`
  (feed `:e <Tab>` in a temp dir, assert a picker frame, type to filter, confirm,
  assert the file opened in the buffer).
- Hermetic temp dir with a known tree (harness temp-file helpers).
- `examples/cmdline-file-picker/` runnable config + sample tree.
- Doc note in the cmdline-completion plan + the picker spec.

## Revision (2026-06-27) — paste-not-execute, a box title, directory preview

Three follow-up changes after the first landing:

1. **Picker box title** (general, reusable): `nx.picker.open(name, { title = … })`
   renders a single title on the box's top border. Threaded core
   (`Menu.title`/`MenuView.title`) → `PickerOpenReq.title` → projection
   (`menu.title`) → `MenuData.title` → **all three clients** (TUI `title_top`, GUI
   `draw_glyph_border`, web `appendGlyphBorder`), styled by the existing
   `MenuStyles.title` (Telescope/FloatTitle).

2. **The cmdline file picker pastes, it does not execute.** `<Tab>` no longer
   cancels the command line — the picker opens OVER it (a `Picker` key context wins
   over Command mode in `feed_matcher`, so the picker grabs input while the `:` line
   stays open). Confirm calls `nx._cmdline_set_arg(path)` → `Editor::cmdline_replace_arg`
   (replaces the argument token in place, reusing `cmdline_complete_token`'s anchor),
   leaving the filled line for the user to run with `<CR>`. Title: "Select file" /
   "Select directory". `line_prefix` dropped from the handoff state.

3. **Directory preview lists contents.** `read_preview_file` falls back to
   `read_preview_dir` (entries, dirs-first, trailing `/`) on BOTH the open error and
   the read error — on Linux a directory opens fine and only fails at read time
   (`EISDIR`), so the read-error path is the one that actually fires.

Tests: `file_command_arg_pastes_the_chosen_path_into_the_open_cmdline`,
`picker_descends_dirs_previews_them_and_cd_lists_only_dirs`,
`open_with_a_title_projects_it_on_the_box`. Example doc updated.

## Revision 2 (2026-06-27) — title polish, multiselect toggle, select-directory

- **Centered title**: the picker box title is centered on the top border (TUI
  `.centered()`, GUI `draw_glyph_border(center_title)`, web `appendGlyphBorder(centerTitle)`).
  Floats keep their left-aligned titles (the flag is opt-in per call).
- **Built-in picker titles**: `files` → "Find Files", `live_grep` → "Live Grep",
  `buffers` → "Buffers" (a `title` field on the source spec).
- **`multiselect` toggle**: `nx.picker.open{ multiselect = false }` (also a source
  field) makes `<Tab>` marking a no-op — threaded `Menu.multiselect` and gated in
  `apply_picker_action("toggle_select")`. The cmdline file picker sets it false.
- **`<select directory>` row**: inside a sub-directory (non-empty base, empty leaf)
  the cmdline file source pushes a first row that pastes the directory itself
  (`is_dir = false`, path = base) instead of descending — `:cd src/` in one go.
- Also: a wildmenu *edit* now only narrows the same token (anchor unchanged); moving
  past it closes the menu, so the space after a command never auto-opens the next
  completion (the file picker / `:set` list) — that needs an explicit `<Tab>`.

## Notes / invariants

- No blocking IO: the source is `nx.fs`-only (async), so it works native, over
  the daemon, and on wasm/OPFS.
- The picker confirm re-executes through `nx.cmd`, so `'switchbuf'`, splits,
  tabs, and modifiers all behave exactly as typing the command would.
- `<Esc>` in the picker cancels the whole gesture (the `:` line was already
  dismissed); v1 does not restore the half-typed command line.
