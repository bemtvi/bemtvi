//! How an editor session starts: [`SessionOptions`], and the single-file preset. What a
//! host's toolkit has nothing to do with, so every host (`bemtvi-mitsuami`'s view, or
//! another) starts its editors the same way.

use anyhow::Result;
use bemtvi_server::ConfigSource;

use crate::remote::ConnectTarget;
use crate::session::{spawn_session_with, Config, Launch, Shada};
use crate::Session;

/// The pickers [`SessionOptions::single_file`] takes away: those that reach past the one
/// file — the open buffers, the workspace's diagnostics, finding files, and grepping
/// them.
pub const SINGLE_FILE_DISABLED_PICKERS: &[&str] = &["buffers", "diagnostics", "files", "live_grep"];

/// The commands [`SessionOptions::single_file`] disables: what the single-file lock
/// doesn't already cover. Running code (`:lua`, `:=`, `:source`, `:command`, and the
/// autocmd family: `:autocmd`, `:augroup`, `:doautocmd`), running processes
/// (`:terminal`, `:make`, `:grep` and their location-list twins, and `:TSInstall` /
/// `:TSUpdate`, which download a grammar and compile it), the working directory
/// (`:cd`, `:tcd`, `:lcd`), other sessions (`:connect`, `:reconnect`, `:disconnect`,
/// `:workspace`, `:rshada`, `:wshada`), and reading other files into a list
/// (`:vimgrep`, `:cfile` and kin). Opening another file needs nothing here: the lock
/// refuses it, and `:e!` (revert) keeps working. (The preset's plugin manager and LSP
/// take their own commands with them.)
pub const SINGLE_FILE_DISABLED_COMMANDS: &[&str] = &[
    "lua",
    "=",
    "source",
    "command",
    "autocmd",
    "augroup",
    "doautocmd",
    "terminal",
    "make",
    "grep",
    "lmake",
    "lgrep",
    "TSInstall",
    "TSUpdate",
    "cd",
    "tcd",
    "lcd",
    "connect",
    "reconnect",
    "disconnect",
    "workspace",
    "rshada",
    "wshada",
    "vimgrep",
    "vimgrepadd",
    "lvimgrep",
    "lvimgrepadd",
    "cfile",
    "cgetfile",
    "caddfile",
    "lfile",
    "lgetfile",
    "laddfile",
];

/// What an editor's [`Session`] starts on: a file, a daemon to route the
/// fs/process/watch/LSP seams to, its config and its shada. Unlike `bemtvi-gui`'s,
/// it shows no intro screen unless asked.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    file: Option<String>,
    target: Option<ConnectTarget>,
    config_source: ConfigSource,
    launch: Launch,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            file: None,
            target: None,
            config_source: ConfigSource::Local,
            launch: Launch::default(),
        }
    }
}

impl SessionOptions {
    pub fn new() -> SessionOptions {
        SessionOptions::default()
    }

    /// An editor for the one file `path` and nothing else — the preset for an app that
    /// embeds the editor to edit a given file:
    ///
    /// - it's locked to the file ([`Self::lock_to_file`]): no other file opens, shows or
    ///   gets written, and no new file buffer is made;
    /// - the commands in [`SINGLE_FILE_DISABLED_COMMANDS`] are disabled: no Lua or
    ///   processes run, no directory or session changes;
    /// - user commands come only from the config and the plugins loaded at startup
    ///   ([`Self::user_commands_from_startup_only`]);
    /// - the plugin manager and LSP are off ([`Self::disable_plugin_manager`],
    ///   [`Self::disable_lsp`]);
    /// - the pickers in [`SINGLE_FILE_DISABLED_PICKERS`] are taken away.
    ///
    /// The rest is as [`Self::new`] sets it, and an app can change it: `config`, `shada`,
    /// `host`, more commands to disable, or [`Self::enable_commands`] to give some back.
    pub fn single_file(path: impl Into<String>) -> SessionOptions {
        SessionOptions::new()
            .file(path)
            .lock_to_file(true)
            .user_commands_from_startup_only(true)
            .disable_commands(SINGLE_FILE_DISABLED_COMMANDS.iter().copied())
            .disable_plugin_manager()
            .disable_lsp()
            .disable_pickers(SINGLE_FILE_DISABLED_PICKERS.iter().copied())
    }

    /// Turn the plugin manager off: `btv.plugins` refuses every call (`E5805`), so the
    /// config declares, loads, installs and updates no plugin through it, and the
    /// `:Plugin*` commands are disabled. Plugins under the config's `pack/*/start/` still
    /// load. `btv.plugins.disabled()` tells a config.
    pub fn disable_plugin_manager(mut self) -> SessionOptions {
        self.launch.disable_plugin_manager = true;
        self
    }

    /// Turn LSP off: no language server starts, `btv.lsp.enable` / `start` / `restart`
    /// raise `E5804`, and the `:Lsp*` commands are disabled. `btv.lsp.disabled()` tells a
    /// config.
    pub fn disable_lsp(mut self) -> SessionOptions {
        self.launch.disable_lsp = true;
        self
    }

    /// Take pickers away, by source name (`"files"`, `"live_grep"`, `"buffers"`,
    /// `"diagnostics"`, or a plugin's): `btv.picker.open` raises `E5803` for one, and the
    /// `commands` and `pickers` lists leave it out. A name that isn't `[A-Za-z0-9_]`
    /// fails the session's start.
    pub fn disable_pickers<I, S>(mut self, names: I) -> SessionOptions
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.launch
            .disabled_pickers
            .extend(names.into_iter().map(Into::into));
        self
    }

    /// The file the first buffer opens.
    pub fn file(mut self, path: impl Into<String>) -> SessionOptions {
        self.file = Some(path.into());
        self
    }

    /// Run the editor here but its fs/process/watch/LSP on a `bemtvi --daemon`
    /// (the edit-host split), as `bemtvi-gui`'s `:connect` does.
    pub fn connect(mut self, target: ConnectTarget) -> SessionOptions {
        self.target = Some(target);
        self
    }

    /// For a daemon session, whose config runs: this machine's (the default) or the
    /// daemon's.
    pub fn config_source(mut self, source: ConfigSource) -> SessionOptions {
        self.config_source = source;
        self
    }

    /// Which config the editor runs (the user's own by default).
    pub fn config(mut self, config: Config) -> SessionOptions {
        self.launch.config = config;
        self
    }

    /// Where its shada lives (the user's own store by default).
    pub fn shada(mut self, shada: Shada) -> SessionOptions {
        self.launch.shada = shada;
        self
    }

    /// Show the intro screen when it opens no file.
    pub fn intro(mut self, on: bool) -> SessionOptions {
        self.launch.intro = on;
        self
    }

    /// Take commands away from the editor: each (by any spelling: `"e"` disables
    /// `:edit`) is refused from then on with `E5800`, wherever it's run from, a plugin's
    /// `btv.cmd` included, and completion leaves it out. Adds to what's already
    /// disabled. A user command can be disabled by its name too.
    pub fn disable_commands<I, S>(mut self, names: I) -> SessionOptions
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.launch
            .disabled_commands
            .extend(names.into_iter().map(Into::into));
        self
    }

    /// Take user commands only from the editor's config and the plugins loaded with it.
    /// Once startup ends, a command defined by any other code is refused (`E5802`): a
    /// plugin loaded later, Lua the user typed, a file sourced from elsewhere. A startup
    /// plugin may still define one later (on an event, say): what counts is the file the
    /// defining code is in. The app's own `EditorHandle::exec_lua` may too.
    pub fn user_commands_from_startup_only(mut self, on: bool) -> SessionOptions {
        self.launch.startup_user_commands_only = on;
        self
    }

    /// Lock the editor to its file ([`Self::file`], which it then needs): no other file
    /// is opened, switched to or written, and no new file buffer made — whether a
    /// command, a key, a plugin or a jump asks — with `E5801`. Views, panels, floats,
    /// quickfix lists and terminals work as ever.
    pub fn lock_to_file(mut self, on: bool) -> SessionOptions {
        self.launch.single_file = on;
        self
    }

    /// Give back commands disabled before (by [`Self::single_file`], say), by any
    /// spelling.
    pub fn enable_commands<I, S>(mut self, names: I) -> SessionOptions
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        use bemtvi_core::ex_names::canonical_or_self;
        let enabled: Vec<String> = names
            .into_iter()
            .map(|name| canonical_or_self(name.as_ref()).to_owned())
            .collect();
        self.launch
            .disabled_commands
            .retain(|name| !enabled.iter().any(|e| e == canonical_or_self(name)));
        self
    }

    /// The app's id, which the editor's Lua sees as `btv.host.name()`, and which lets a
    /// config or plugin message the app with `btv.host.send` (an [`Effect::Host`](crate::Effect::Host)).
    pub fn host(mut self, id: impl Into<String>) -> SessionOptions {
        self.launch.host = Some(id.into());
        self
    }

    /// Let `:connect`, `:workspace` and `btv.session.reconnect` swap the editor onto
    /// another session, as in `bemtvi-gui` (off by default: an app embedding the editor
    /// usually means it to stay on the session it gave it). Off, `:workspace` isn't a
    /// command and a `:connect` that would swap fails, saying why.
    pub fn session_switching(mut self, on: bool) -> SessionOptions {
        self.launch.session_switching = on;
        self
    }

    /// Start the session's server on its own thread. **Blocking** for a daemon session,
    /// which waits for the connection, so a refused one is an `Err` here; as is a shada
    /// namespace that isn't a safe name. The file of a daemon target (`host/file`)
    /// opens when no [`Self::file`] was given.
    pub fn spawn(self) -> Result<Session> {
        let file = self
            .file
            .or_else(|| self.target.as_ref().and_then(ConnectTarget::embedded_file));
        spawn_session_with(self.target, file, self.config_source, self.launch)
    }
}
