# Disabled commands, and a single-file editor

**Goal.** An app embedding the editor (`bemtvi_gui::embed`) can take commands away from
it, `:edit` say, where they make no sense for how it's used. On top of that there's a
preset for the common embedded case: an editor for **one given file**. It can't open,
switch to or write any other file, and the user can't run Lua or define commands. The
plugins loaded at startup keep working, commands included.

Decided with the user (2026-10-27):

- A disabled command is refused **on every route**: typed, from a mapping, from a
  plugin's `btv.cmd`, from an autocmd's `command=`. In a single-file editor a plugin
  that tries to open another file gets a loud error too.
- **No user Lua** means both of these: the commands that run code (`:lua`, `:lua=`,
  `:=`, `:source`, `:command`) are disabled; and user commands may only be defined by
  the config and the plugins loaded with it (told by the file the defining code is in,
  see below). Any other code's is refused.
- **Single-file** covers keys too (`gf`, `<C-^>`, a picker's or an LSP jump's other
  file), not just the command table.

## What the code gives today

Survey of the dispatch (2026-10-27):

- Every ex command line goes through `Editor::execute_ex` → `execute_ex_one`
  (`crates/bemtvi-core/src/editor/ex.rs`). That covers typed lines, mappings and
  `:normal` (which feed keys to the cmdline), `btv.cmd` / autocmd `command=` / string
  user-command bodies (`Sequenced::Command` → `Editor::command`), the `btv_command`
  RPC, and `:g`. A name the core doesn't know is deferred to the server's
  `resolve_command` (`crates/bemtvi-server/src/excmd.rs`): server built-ins, then user
  commands, then `E492`. `:silent X` is deferred whole, and the server runs `X` back
  through `execute_ex`. **So one check in `execute_ex_one` sees every command**, with
  two exceptions: `:tab X` and `:vertical X` re-parse `X` and dispatch it directly.
- **There are no canonical names.** Both dispatchers match literal spellings
  (`"e" | "edit"`, `_ if matches!(base, "cd" | "chd" | "chdi" | "chdir")`), with no
  table. Disabling `edit` would need every spelling of it, and a spelling added to an
  arm later would slip past the check without anyone noticing.
- User commands live in Lua tables (`btv._user_commands`, `prelude/autocmd.lua`), with
  no record of when they were made.
- Nothing gates commands today: the `<expr>` textlock and the call-context lock are
  Lua-side and only see `btv.cmd`.

## Design

### Canonical command names (the seam)

One table in the core, `editor/ex_names.rs`, maps every built-in spelling (core and
server) to its canonical name: `e`/`edit` → `edit`, `cd`/`chd`/`chdi`/`chdir` → `cd`.
Arms that already share behaviour share a canonical name (`tabnew`/`tabedit`). **Both
dispatchers match on the canonical name**, so the table is the only list of spellings:
a spelling that isn't in it doesn't dispatch at all, and so can't get around a disabled
command. A name not in the table (a user command) is its own canonical name.

### Disabled commands

The editor holds a set of disabled canonical names. `execute_ex_one` refuses one
before it dispatches, with `E5800: :edit is disabled in this editor`. That's an
`E###` error, so it stops the rest of a `|` chain, and bare `:silent` doesn't hide it.
`:tab` and `:vertical` check what they dispatch. A name the caller disables is
canonicalized too, so disabling `e` or `tabnew` disables the command whatever it's
spelled.

Where it's set: `ServerInit::disabled_commands`, then
`SessionOptions::disable_commands([...])` for an embedding app. Lua can read it
(`btv.command_disabled(name)`), so command-line completion leaves disabled commands
out and a config can tell.

### User commands from startup only

Decided with the user (2026-10-27): **by provenance, not by time.** Lua keeps where
every function was defined, and bemtvi names every chunk after its file: `init.lua` and
each plugin's `plugin/*.lua` run as `@<path>` (`lifecycle.rs`), the plugin manager
sources with `loadstring(content, "@" .. f)`, `require` goes through Lua's own
`package.path` searcher (also `@<path>`), and the prelude's chunks are
`bemtvi:prelude/…`. `btv.shada.plugin` already attributes calls to plugins this way
(`btv._runtime_paths`, `btv.utils.caller_source`).

- As startup ends (where `PluginsLoaded` fires, exactly once a session), the runtimepath
  of that moment is sealed as the **startup roots**: the config dir, the `pack/*/start`
  plugins, the system-plugin tier, the plugins `btv.plugins` loaded eagerly.
- From then on `btv.user_command.create` / `buf_create` find the code defining the
  command: the first stack frame that is neither C nor the prelude's own. It may if that
  is a file under a startup root (**whenever it runs**: a startup plugin's autocmd
  defining a command later counts), the prelude itself, or the client's
  `nvim_exec_lua` (the app embedding the editor). Otherwise `E5802`, naming the command
  and where the code comes from: a plugin loaded later, typed `:lua`, a file
  `:source`d from elsewhere.
- Two chunk kinds get names so they can be told apart from files: `nvim_exec_lua`
  (`=nvim_exec_lua`) and a typed `:lua` (`=:lua`). Before, mlua named both after their
  Rust call site, which reads like a path.
- `:command` defines none after startup: it runs deferred, from the editor, so the code
  that asked for it is gone. During startup it works as ever.
- Before the seal everything is allowed, so a lazy plugin triggered during startup
  counts as a startup one.
- A policy, not a sandbox: Lua can name a chunk what it likes. Plugins are trusted
  code; what's kept out is the user's own Lua (with `:lua` disabled they have no way to
  run any) and plugins that load later.

### The single-file lock

The editor is locked to one buffer: the file it started on (`ServerInit::single_file`,
which needs `ServerInit::file`; the startup buffer is the editor's first, in a daemon
session too, where the fetch fills that same id). As **file buffers** (`'buftype'` `""`),
only that one may be opened, shown or written. Everything else fails with
`E5801: this editor only edits <file>`. Every other kind of buffer (a view, a panel, a
doc float, a quickfix list, a terminal) works as ever.

Survey of the core (2026-10-27), and where the guards went:

- A path is the locked file's when `find_buffer_by_path(path)` (the cwd-aware matching
  every open already uses) finds the locked buffer, so there's no second path
  comparison to drift.
- **Opening by path** funnels through `edit_in_current_window` (`:e`, `btv.open`, the
  picker, LSP and quickfix jumps), `open_buffer` (`:tabedit`, a jump into a tab) and the
  kernel `load_new_buffer`; plus the loaders that bypass it, `open_buffer_for_restore`
  and `ensure_buffer_loaded` (LSP workspace edits). All five refuse another path.
  `ex_tabnew` and `ex_split` refuse up front, since one falls back to an empty buffer
  on a failed load and the other splits before it edits.
- **New file buffers**: `:enew`, `:new` / `:vnew`, `:tabnew`.
- **Switching** to an existing buffer goes through `switch_buffer` (`:b`, `<C-^>`,
  `:bnext`, jumplist and mark jumps) or the picker's `open_buffer_in_tab` /
  `open_buffer_in_split`: refused for another file buffer.
- **Writes**: `:w other` (and `:wq other`) is refused; it would also rename the buffer
  to that name. `:w` and `:w <the file>` work.
- **Deleting** the locked buffer (`:bd`) is refused: it would leave an empty buffer.
- `gf` / `gF` don't exist in bemtvi, so there's nothing to guard there.
- Not guarded, on purpose: `set_window_buffer` (`nvim_win_set_buf`, docks and panels
  mount their own buffers through it) and `create_buffer` (`nvim_create_buf`, which only
  the client can call). In a locked editor the only other file buffers that can exist
  are ones the app made itself. `create_file_buffer` (an LSP create-file edit) makes a
  buffer it shows nowhere; it isn't guarded either.

### The preset

`SessionOptions::single_file(path)` is the file plus the lock, plus startup-only user
commands, plus `SINGLE_FILE_DISABLED_COMMANDS`: what the lock doesn't already cover. That
means the code runners (`lua`, `=`, `source`, `command`), the process runners
(`terminal`, `make`, `grep`, `lmake`, `lgrep`, and `TSInstall` / `TSUpdate`, added in
review), the working directory (`cd`, `tcd`, `lcd`), sessions (`connect`, `reconnect`, `disconnect`, `workspace`, `rshada`,
`wshada`), and reading other files into a list (`vimgrep`, `vimgrepadd`, `lvimgrep`,
`lvimgrepadd`, and the `cfile` family). An app can add to the set, or give some back with
`enable_commands`.

Changed while building it: the plan also disabled the file commands (`edit`, `enew`,
`new`, `tabedit`, `drop`) for a clearer error than the lock's. The lock already says
`E5801: this editor only edits <file>`, and disabling `edit` would also have taken `:e!`
(revert), which a single-file editor wants. So they stay enabled, and the lock answers
them.

### Added after phase 4 (2026-10-27)

The user asked the preset to take away more:

- **The plugin manager, all of it** (decided with the user: not just its UI).
  `ServerInit::disable_plugin_manager` swaps the public `btv.plugins` for a stand-in that
  raises `E5805` for every call but `disabled()`, while the prelude's own reads of its
  private `_` fields (shada attribution, the test runner's idle wait, view restore) still
  reach it, and the manager's internals (which hold a local `M`) keep firing
  `PluginsLoaded`, which the user-command seal needs. The first-run bootstrap offer is
  skipped. Its `:Plugin*` commands are disabled with it (`PLUGIN_MANAGER_COMMANDS`).
- **LSP, refusing loudly** (decided with the user, per the no-silent-stubs rule).
  `ServerInit::disable_lsp`: `btv.lsp.enable` / `start` / `restart` raise `E5804`, and so
  does the one place every start passes (`start_resolved`). The `:Lsp*` commands are
  disabled (`LSP_COMMANDS`). `btv.lsp.disabled()` lets a shared config check.
- **Four pickers**: `buffers`, `diagnostics`, `files`, `live_grep`
  (`SINGLE_FILE_DISABLED_PICKERS`). `ServerInit::disabled_pickers` refuses them at
  `btv.picker.open` with `E5803` (every entry point, keymaps and the palette included,
  goes through it), and the `commands` and `pickers` lists leave them out. A name must be
  `[A-Za-z0-9_]`, since it's passed into the prelude's switch.
- **The autocmd family**: `:autocmd`, `:augroup`, `:doautocmd` join the preset's disabled
  commands. A config's or plugin's `btv.on` is unaffected.

## Phases

1. **Canonical names + disabled commands.** The table; both dispatchers on canonical
   names; the disabled set in the core (`execute_ex_one`, `:tab`, `:vertical`);
   `ServerInit` / `SessionOptions`; `btv.command_disabled`; completion leaving them
   out. Tests (server harness): each route refused (typed, mapping, `btv.cmd`, autocmd
   `command=`, `:silent`, `:tab`, `:vertical`, a `|` chain stopping), every spelling,
   a user command disabled by name, and everything not disabled still working. The
   existing suites guard the refactor.
2. **Startup-only user commands, by provenance.** The seal at `PluginsLoaded`, the
   origin check in `user_command.create` / `buf_create` and `:command`, the chunk names,
   `ServerInit` / `SessionOptions`. Tests: the config's and a startup plugin's commands
   work, both may define one later from an autocmd, the client may, typed `:lua`, a
   plugin loaded later, a file sourced from elsewhere and `:command` may not, and with
   the policy off anything may.
3. **The single-file lock.** In the core, at the opens, new buffers, switches, writes
   and the delete above; `ServerInit::single_file`. Tests: `:e`, `:drop`, `:tab edit`,
   `:sp other` (no stray split), `:tabedit`, `:enew` / `:new` / `:vnew` / `:tabnew`,
   `btv.open`, a quickfix jump, `:b` / `:bnext` to an app-made buffer, `:w other`, `:bd`
   refused; `:sp`, `:e <the file>`, `:w`, the quickfix window and `:messages` working;
   unlocked by default; a lock with no file fails the start.
4. **The preset, example and docs.** `SessionOptions::single_file`, `lock_to_file`,
   `enable_commands`; embed tests for the whole preset and for giving commands back;
   `examples/single-file/` (spec included) with a `single_file` cargo example; the
   embedding book page.

Each phase is committed and paused for review.
