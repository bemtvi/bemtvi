# Workspaces

A **workspace** is the VSCode "open a folder" model brought to a modal editor:
open a directory *as a workspace* — `nxvim --workspace <dir>` in the terminal, or
the `:workspace [dir]` command in the GUI — and it becomes a persistent project
session. The window/split/dock layout, the open files (with cursor and scroll),
and any modified scratch buffers are saved on exit and restored on the next
launch — and the directory becomes the working directory, so relative paths,
`:find`, and project plugins resolve against the project root.

It is opt-in: an ordinary `nxvim <dir>` does *not* start a workspace — you ask for
one with the flag or command. vim/neovim have `:mksession` for something
adjacent, but it is manual on both ends (you save and load the session yourself)
and global. Once a workspace is open, the save-on-exit / restore-on-launch is
automatic, per-directory, and carries its own option overrides.

## Opening a workspace

`--workspace` takes the directory as its value (a bare `--workspace` uses the
current directory). The positional `TARGET` is a *separate* optional file to open:

```sh
nxvim --workspace ~/code/myproject          # open a directory as a workspace
nxvim --workspace ~/code/myproject README.md  # …and open a file in it
nxvim --workspace                           # the current directory
```

`--workspace` does three things:

- **Derives a per-directory shada namespace** from the directory, so each
  project keeps its own session and history, isolated from every other workspace
  and from the global store. (An explicit `--shada-namespace` overrides the
  derived one.)
- **Enables session capture and restore** with no plugin opt-in — the layout is
  saved on exit and replayed on the next launch.
- **Changes the working directory** to the workspace root (by default; pass
  [`--workspace-no-cwd`](#keeping-the-launch-directory) to keep the launch
  directory). A relative `TARGET` file then resolves against the workspace root.

The native GUI also exposes a client-side **`:workspace [dir]`** command: a bare
`:workspace` reopens the current directory as a workspace, and `:workspace <dir>`
switches the running window to that directory's session. (Like `:connect`, this
is a GUI-client command — the server knows nothing of it.)

## What gets restored

A workspace session round-trips, across restarts:

- **Tabs** — count and order.
- **The split layout** — the full nesting and proportional sizes of every
  window.
- **One file per window leaf** — its path, cursor line/column, and scroll
  position; the focused window is re-selected.
- **Docks** — left/right/top/bottom docks, their size and shown/hidden state,
  and any file-backed content.
- **Modified unnamed buffers** — a `[No Name]` buffer with unsaved edits is
  saved with its contents (under `'workspacepersistunnamed'`, on by default).

The quickfix and location lists are deliberately *not* part of the session
(they are transient search results, not layout); marks and registers ride the
ordinary shada store within the workspace's namespace.

## Keeping the launch directory — `--workspace-no-cwd`

By default a `--workspace` launch makes the workspace directory the process
working directory: the editor cds into the root at **boot**, before it opens any
file or restores the session, so relative paths and the session's (relative,
portable) buffer paths resolve against the root. Pass `--workspace-no-cwd` to keep
the directory you launched from instead:

```sh
nxvim --workspace ~/code/myproject --workspace-no-cwd
```

The cd is a launch-time decision (a CLI flag), not a config option — there is no
`init.lua` knob, so the working directory is settled from the first instruction.
`--workspace-no-cwd` has no effect without `--workspace`.

## Per-workspace option overrides — `nx.wso`

`nx.wso` is a per-workspace overlay over the global option tier: a value set
here **takes precedence over the global value** for this workspace only, and is
persisted with the session. It is the place to pin project-specific settings —
a tab width, a colorscheme toggle, search case sensitivity — without touching
your global config.

```lua
nx.wso.tabstop = 2             -- this project indents with 2
nx.wso.ignorecase = true       -- case-insensitive search here
nx.wso.tabstop = nil           -- clear the override; revert to the global value
```

Only **global** options can be workspace-overridden (not window- or
buffer-local ones), and the name and value type are validated on write. The
resolution order is simply: the **`nx.wso`** overlay when present, otherwise the
**global** value (`nx.o` / `:set`).

## Workspaces and the remote daemon

A workspace pairs with the [edit-host split](../edit-host-split.md): launch
`--workspace` together with `--connect-daemon` and the workspace root is resolved
from the daemon's working directory after connect, so you edit a remote project
as a first-class workspace with the layout restored locally. (`--workspace` is a
client-side concept and does not combine with running *as* a daemon.)
