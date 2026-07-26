# Git status in nxvim-tree, without moving the filename

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

`nx.git.status` speaks porcelain `XY` and reports only *changes*: gix's status platform
never emits ignored paths unless the dirwalk is asked for them (`emit_ignored`). There is
no ignored information anywhere in `nx.*` today.

The tempting shortcut — have the plugin read `.gitignore` and match globs itself — is
exactly the heuristic CLAUDE.md forbids: it re-derives something the engine knows
structurally, and it silently rots (nested `.gitignore`s, negations, `core.excludesFile`,
`.git/info/exclude`). So Phase 1 closes the gap at the canonical layer and Phase 2 is a
pure plugin change consuming it.

`gix` supports exactly what a file tree wants: `emit_ignored(Some(CollapseDirectory))`
reports `target/` as **one** entry rather than 50k paths.

## Phase 1 — `nx.git.status(path, { ignored = true })` (nxvim repo)

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
| `crates/nxvim-lua/src/ops.rs` | `GitJob::Status { path, ignored: bool }` + doc |
| `crates/nxvim-lua/src/install.rs` | Lua table → job: read `ignored` (absent ⇒ `false`) |
| `crates/nxvim-lua/src/gitwire.rs` | daemon wire: encode + decode the new field |
| `crates/nxvim-git/src/lib.rs` | `emit_ignored` when asked; map `Status::Ignored` → `!!` |
| `crates/nxvim-lua/src/prelude/git.lua` | `status(path, opts)`, documented |

Tests (`--workspace`): `tests/git.rs` — ignored entries appear only when asked, a
collapsed ignored directory is one entry, an ignored file inside a tracked directory
reads `!!`; `tests/daemon_git.rs` — the same over the daemon wire (the remote session is
tier-1). Mutation-check each by breaking the engine and watching them fail.

## Phase 2 — the tree's presentation (nxvim-tree repo)

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

Verified by the plugin's own suite (`nxvim --test-plugin .`) plus a throwaway real-session
check that the toggle survives a workspace restart.

## Status

- [x] Phase 1 — engine capability
- [ ] Phase 2 — tree presentation
