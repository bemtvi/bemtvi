# Git status in bemtvi-tree, without moving the filename

**Goal.** The file explorer differentiates entries by git status — ignored / untracked /
added / modified / staged / deleted — with the **filename's start column fixed**: nothing
is ever inserted between the icon and the name, and the tree never jumps horizontally as
statuses come and go.

Requested presentation (confirmed with the user):

- **Name colour + gutter sign**, with the **gutter always reserved** so the tree's text
  never shifts. Today `signcolumn` is `auto:1` on the tree window, so the whole sidebar
  slides one column right the moment the first sign appears — the tree pins `yes:1`
  instead and the sign column is simply always there.
- **Ignored files are dimmed**, with a **config flag to hide them** instead, plus a
  **keybind that toggles show/hide at runtime**, persisted in the workspace session the
  same way the `H` dotfile toggle now is.

## Why this needs a core change first

`btv.git.status` speaks porcelain `XY` and reports only *changes*: gix's status platform
never emits ignored paths unless the dirwalk is asked for them (`emit_ignored`). There is
no ignored information anywhere in `btv.*` today.

The tempting shortcut — have the plugin read `.gitignore` and match globs itself — is
exactly the heuristic CLAUDE.md forbids: it re-derives something the engine knows
structurally, and it silently rots (nested `.gitignore`s, negations, `core.excludesFile`,
`.git/info/exclude`). So Phase 1 closes the gap at the canonical layer and Phase 2 is a
pure plugin change consuming it.

`gix` supports exactly what a file tree wants: `emit_ignored(Some(CollapseDirectory))`
reports `target/` as **one** entry rather than 50k paths.

## Phase 1 — `btv.git.status(path, { ignored = true })` (bemtvi repo)

Opt-in, because emitting ignored costs a full dirwalk of directories git otherwise prunes.
Default off ⇒ every existing consumer is byte-identical.

Ignored entries are spelled `!!` — both porcelain columns — matching
`git status --porcelain --ignored`, so no new entry field is needed and `classify()` reads
it like any other code. A collapsed directory arrives as its plain path (no trailing
slash, consistent with how collapsed *untracked* directories already arrive); a consumer
resolves directory-ness from its own model, and descendants match by path prefix.

Touchpoints:

| file | change |
| --- | --- |
| `crates/bemtvi-lua/src/ops.rs` | `GitJob::Status { path, ignored: bool }` + doc |
| `crates/bemtvi-lua/src/install.rs` | Lua table → job: read `ignored` (absent ⇒ `false`) |
| `crates/bemtvi-lua/src/gitwire.rs` | daemon wire: encode + decode the new field |
| `crates/bemtvi-git/src/lib.rs` | `emit_ignored` when asked; map `Status::Ignored` → `!!` |
| `crates/bemtvi-lua/src/prelude/git.lua` | `status(path, opts)`, documented |

Tests (`--workspace`): `tests/git.rs` — ignored entries appear only when asked, a
collapsed ignored directory is one entry, an ignored file inside a tracked directory
reads `!!`; `tests/daemon_git.rs` — the same over the daemon wire (the remote session is
tier-1). Mutation-check each by breaking the engine and watching them fail.

## Phase 2 — the tree's presentation (bemtvi-tree repo)

1. **Name colour.** The git decorator already returns `{ hl = }` and `render.lua` already
   paints it over the name range only — the decorator just never set it. Set it, and add
   `NvimTreeGitIgnored` (dim, italic) to the fallback palette.
2. **Stable gutter.** Pin `signcolumn = "yes:1"` on the tree window next to the existing
   `cursorline` / `winhighlight` chrome, so the sign column is always reserved.
3. **Ignored.** Fetch with `{ ignored = true }`; index ignored paths into a prefix-matched
   set so descendants of a collapsed `target/` are ignored too.
4. **Hide/show.** `git_ignored = "dim" | "hide"` in config, a `toggle_git_ignored` action
   (default `I`, nvim-tree's key) that flips it live, and the current value in the session
   snapshot — the same preference treatment `hidden` just got.

Verified by the plugin's own suite (`bemtvi --test-plugin .`) plus a throwaway real-session
check that the toggle survives a workspace restart.

## Status

- [x] Phase 1 — engine capability
- [x] Phase 2 — tree presentation

Notes from the build:

- `signcolumn` normalizes `yes:1` → `yes` on read-back, so the tree sets `"yes"`.
- The gutter is pinned from the git module's **first successful status**, not at build time:
  that is the only point where "we are in a repo" is known. A rebuild inside a live session
  re-pins it from the window-chrome step, since the fresh window starts at the default.
- `sign_hl_group` turned out to be painted end to end (`merged_sign_cells` resolves it into
  the sign cell's style) — `btv.buf.set_extmark`'s doc comment claimed otherwise and was
  stale; corrected in `prelude/api.lua` while here.
- Hiding ignored entries is a **render-time** filter, not a scandir filter, so `I` is
  instant and a directory you un-hide keeps its expand state.
