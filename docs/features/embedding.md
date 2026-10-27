# Embedding bemtvi in an app

A [mitsuami](https://mitsuami.roboport.com.br) app can put a bemtvi editor anywhere in
its own layout: a pane beside a sidebar, a tab, a dialog. It's the whole editor, with
its config, its plugins and LSP. The app drives it through a handle, and hears from it
through events. `bemtvi-gui` is itself one of these editors, filling a window.

```sh
cargo run -p bemtvi-gui --example embed
```

runs a small notes app built this way (`crates/bemtvi-gui/examples/embed.rs`), with
`examples/embed/init.lua` as its editor's config.

## The editor in a layout

```rust
use bemtvi_gui::embed::{Config, Editor, EditorEvent, EditorHandle, SessionOptions, Shada};

let editor = EditorHandle::new();
let session = SessionOptions::new()
    .file("notes.md")
    .config(Config::Dir(app_config_dir))      // or Config::User, Config::None
    .shada(Shada::Namespace("myapp".into()))  // or Shada::Global, Shada::Off
    .host("myapp")
    .spawn()?;

Row::new().children((
    sidebar(),
    Editor::new(session, move || tabs.close_current())   // on_exit
        .handle(editor.clone())
        .on_event(move |event| match event {
            EditorEvent::Host { name, payload } => status.set(format!("{name}: {payload}")),
        }),
))
```

- **The session comes first.** `SessionOptions::spawn` starts the editor's server on
  a thread of its own and fails then, before anything is mounted: a daemon that refused
  the connection (`.connect(target)`, the [edit-host split](edit-host-split.md)), or a
  shada namespace that isn't a safe name.
- **`on_exit` is required.** It's what the app does when the editor quits (`:q` of the
  last window, or its server went away), so a quit editor never sits there showing its
  last frame.
- **It takes the space left over.** The editor's surface has no size of its own: it
  grows into what its row or column has left, stretched across the other way.

## Its config and shada

`Config::User` runs the user's own config, as `bemtvi` does: right for a tool whose
users already run bemtvi. `Config::Dir` runs the app's own directory (its `init.lua`,
and plugins under its `pack/*/start/`). `Config::None` runs no config at all. The
directory and no-config cases leave out the user's system-plugin tier too.

`Shada::Global` shares marks, registers and history with the user's other bemtvi
instances. `Shada::Namespace` keeps a store of its own under the shada directory.
`Shada::Off` persists nothing.

By default the editor stays on the session the app gave it: `:workspace` isn't a
command, and `:connect` or a plugin's `btv.session.reconnect` is refused with a message
saying why. `SessionOptions::session_switching(true)` allows them, as `bemtvi-gui` does.

## Taking things away

An app can take from the editor what makes no sense for how it's used.

- **Commands.** `SessionOptions::disable_commands(["lua", "terminal"])` disables commands
  by any spelling (`e` is `:edit`). A disabled command fails with
  `E5800: :lua is disabled in this editor` wherever it's run from: typed, from a mapping,
  from a plugin's `btv.cmd`, an autocmd, under `:silent` or `:tab`. As an error it also
  stops the rest of a `|` chain. Completion leaves it out, and Lua can ask
  `btv.command_disabled(name)`. A user command can be disabled by its name too.
- **User commands from elsewhere.** `user_commands_from_startup_only(true)` takes user
  commands only from the editor's config and the plugins loaded with it. Once startup
  ends (`PluginsLoaded`), a command defined by any other code fails with `E5802`, naming
  where that code is: a plugin loaded later, typed `:lua`, a file `:source`d from
  elsewhere. What counts is the file the defining code is in, not when it runs, so a
  startup plugin that defines a command later, from an autocmd say, still may. So may the
  app's own `exec_lua`. `:command` defines none after startup: it runs deferred, and the
  code that asked for it is gone. This is a policy, not a sandbox: Lua can name its code
  what it likes, and plugins are trusted code.
- **Other files.** `lock_to_file(true)` locks the editor to its file. Opening, switching to
  or writing any other file fails with `E5801: this editor only edits <file>`, and so does
  a new file buffer. That holds whatever asks: `:e`, `:sp`, `:tabedit`, `:enew`, `:b`,
  `<C-^>`, `btv.open`, the picker, an LSP or quickfix jump, `:w other`, `:bd` of the file
  itself. Views, panels, floats, quickfix lists and terminals work as ever, and so do
  `:e!`, `:w` and `:sp` of the file itself.
- **The plugin manager.** `disable_plugin_manager()` makes every `btv.plugins` call fail
  with `E5805`, so the config declares, loads, installs and updates nothing through it,
  and its `:Plugin*` commands are disabled. Plugins under the config's `pack/*/start/`
  still load. A config can ask `btv.plugins.disabled()`.
- **LSP.** `disable_lsp()` means no language server starts. `btv.lsp.enable`, `start` and
  `restart` fail with `E5804`, and the `:Lsp*` commands are disabled. A config meant to run
  both ways checks `btv.lsp.disabled()` first.
- **Pickers.** `disable_pickers(["files", "live_grep"])` takes pickers away by source name.
  `btv.picker.open` fails with `E5803` for one, and the `commands` and `pickers` lists
  leave it out. A config can ask `btv.picker.disabled(name)`.

### An editor for one file

`SessionOptions::single_file(path)` is all of it, for an app that embeds the editor to
edit one given file:

- it locks to the file and takes user commands only from startup;
- it turns the plugin manager and LSP off;
- it takes away the pickers in `SINGLE_FILE_DISABLED_PICKERS` (`buffers`, `diagnostics`,
  `files`, `live_grep`);
- it disables `SINGLE_FILE_DISABLED_COMMANDS`. That is what runs code or processes
  (`:lua`, `:=`, `:source`, `:command`, the autocmd family `:autocmd` / `:augroup` /
  `:doautocmd`, `:terminal`, `:make`, `:grep`, `:TSInstall`), changes directory or
  session (`:cd`,
  `:connect`, `:workspace`, `:rshada`), or reads other files into a list (`:vimgrep`,
  `:cfile`).

`.enable_commands([...])` gives commands back.

```sh
cargo run -p bemtvi-gui --example single_file [file]
```

runs one, with `examples/single-file/init.lua` as its config.

## Talking to the app

The app names itself with `SessionOptions::host`. Inside the editor that's `btv.host`
(its page is under the API reference):

```lua
if btv.host.name() then                 -- nil when no app embeds the editor
  btv.host.forward({ "changed", "wrote" })
  btv.keymap.set("n", "<leader>p", function()
    btv.host.send("preview", { file = btv.buf.name(0) })
  end)
end
```

`btv.host.send(name, payload)` reaches the app as `EditorEvent::Host { name, payload }`.
`btv.host.forward` sends `changed` (a buffer's name and modified state, once typing
pauses) and `wrote` messages. Neither sends the buffer's text: the app asks for what it
needs. The app's `on_event` handler runs on the UI thread with the editor free, so it
can drive the editor straight back.

The other way, `EditorHandle` has `command` (an ex command, whatever the mode),
`exec_lua` (a future of what the chunk returns), `open(path)`, `focus()`,
`request_quit()` (`:qa`, which refuses while a buffer is unsaved, as a window's close
button wants) and `quit_now()` (`:qall!`).

## Its lifetime

The editor's threads start as it's mounted and are told to stop as it's unmounted:
a `Show` switching it out, a closed tab. Mounting it again starts a new one. The handle
goes on whichever is mounted, one at a time.

`EditorHandle::close()` waits for the threads to end and says whether a server
panicked. An app calls it before its process ends, so the editor's exit sequence (its
shada, plugins' `VimLeave`) lands.

Every editor in the process shares one GPU device, which is never dropped (wgpu's
EGL teardown crashes on Wayland with Mesa otherwise); each editor's own surface and
glyph caches go with it.

## Keys and focus

The window's shortcuts get a key first: a menu item's shortcut is the app's, and the
editor never sees that chord. A mapping on it in the editor never fires. Everything
else goes to the editor while it has focus, Tab included. Control+Tab moves focus on,
so a `<C-Tab>` mapping never fires in any mitsuami app, `bemtvi-gui` included.

The editor's surface is a native child surface on GTK, Kirigami and Windows, drawn
above the window's own content. Nothing the app draws inside the window can overlap
it, so popovers and menus over it have to be the platform's own popup windows, as
GTK's popovers and Kirigami's windowed lists are.

## Versions

mitsuami's types are part of the API (`Editor` is a mitsuami `View`), so the app has to
build against the same mitsuami bemtvi does. For now that's a GitHub revision, pinned
in bemtvi's root `Cargo.toml`.
