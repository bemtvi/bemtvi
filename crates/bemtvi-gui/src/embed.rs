//! Embedding bemtvi in a mitsuami application: the editor as a view an app puts in its
//! own layout, the way it puts a `TextArea` there.
//!
//! [`Editor`] is the whole editor (core, Lua, config, plugins, LSP, the edit-host split)
//! on a [`Session`] the app started, drawn on a `GpuSurface` that fills what the layout
//! gives it. [`EditorHandle`] is the app's way in: quitting it, and the threads behind
//! it. `bemtvi-gui`'s own window ([`crate::run`]) is one of these, full size.
//!
//! ```ignore
//! let editor = EditorHandle::new();
//! let session = SessionOptions::new()
//!     .file("notes.md")
//!     .config(Config::Dir(app_config_dir))
//!     .shada(Shada::Namespace("myapp".into()))
//!     .spawn()?;
//! Row::new().children((
//!     sidebar(),
//!     Editor::new(session, move || tabs.close_current()).handle(editor.clone()),
//! ))
//! ```
//!
//! The editor's threads (its server, and the client IO thread that talks to it) start
//! as it's mounted and are told to stop as it's unmounted; [`EditorHandle::close`] waits
//! for them, so an app that wants the editor's exit sequence (its shada) to land calls
//! it before the process ends.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use bemtvi_rpc::Rpc;
pub use bemtvi_server::ConfigSource;
use mitsuami::prelude::{on_cleanup, NodeId, Owner, Ui, View};
pub use rmpv::Value;

pub use crate::remote::ConnectTarget;
use crate::session::{spawn_session_with, Launch};
pub use crate::session::{Config, Shada};
use crate::{spawn_io, App, GuiConfig, IoParts, Session};

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

/// What an embedded editor's [`Session`] starts on: a file, a daemon to route the
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
    /// config or plugin message the app with `btv.host.send` (an [`EditorEvent::Host`]).
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

/// An editor, for a mitsuami view tree. Built on a running [`Session`] (see
/// [`crate::spawn_session`]), so a failed start — a daemon that refused the connection —
/// is the app's to handle before anything is mounted.
///
/// `on_exit` is what the app does when the editor quits (`:qa`, or its server went
/// away): close the pane, the tab, or the window. It's required, so an editor that quit
/// can't be left showing its last frame.
pub struct Editor {
    session: Session,
    on_exit: Rc<dyn Fn()>,
    handle: Option<EditorHandle>,
    config: GuiConfig,
    open_dir: Option<PathBuf>,
    config_source: ConfigSource,
    autofocus: bool,
    label: String,
    native_dialogs: bool,
    on_event: Option<Rc<dyn Fn(EditorEvent)>>,
}

/// What the editor tells the app embedding it, besides that it quit (`on_exit`).
#[derive(Clone, Debug, PartialEq)]
pub enum EditorEvent {
    /// A config or plugin called `btv.host.send(name, payload)`: a message whose meaning
    /// the app and its editor config agree on. `btv.host.forward` sends `"changed"` and
    /// `"wrote"` ones.
    Host { name: String, payload: Value },
}

impl Editor {
    pub fn new(session: Session, on_exit: impl Fn() + 'static) -> Editor {
        Editor {
            session,
            on_exit: Rc::new(on_exit),
            handle: None,
            config: GuiConfig::default(),
            open_dir: None,
            config_source: ConfigSource::Local,
            autofocus: false,
            label: "bemtvi".to_owned(),
            native_dialogs: true,
            on_event: None,
        }
    }

    /// What the app does with what the editor tells it ([`EditorEvent`]). Called on the
    /// UI thread, with the editor free to be driven again through its handle. Without
    /// it, a `btv.host.send` reports in the editor that nothing heard it.
    pub fn on_event(mut self, on_event: impl Fn(EditorEvent) + 'static) -> Editor {
        self.on_event = Some(Rc::new(on_event));
        self
    }

    /// Whether `<CR>` over a `:e`/`:w`-family command line pops the platform's file
    /// dialog — `:e` and `:w` with no file, `:e` of a directory, `:wo` and the `…o` open
    /// family — instead of running the command as typed (on by default; never in a
    /// daemon session, whose files aren't on this machine).
    pub fn native_dialogs(mut self, on: bool) -> Editor {
        self.native_dialogs = on;
        self
    }

    /// The app's handle on this editor, once it's mounted.
    pub fn handle(mut self, handle: EditorHandle) -> Editor {
        self.handle = Some(handle);
        self
    }

    /// The font, its size and how wide glyphs are fit. The editor reads no environment:
    /// `bemtvi-gui` builds this from its flags and `BEMTVI_GUI_*`. A config's
    /// `'guifont'` still wins.
    pub fn config(mut self, config: GuiConfig) -> Editor {
        self.config = config;
        self
    }

    /// Opens the platform's file picker at `dir` once the editor shows, instead of the
    /// server's listing of it (what `bemtvi-gui somedir` does).
    pub fn open_dir(mut self, dir: Option<PathBuf>) -> Editor {
        self.open_dir = dir;
        self
    }

    /// Which config a later `:connect` session runs (see `--remote-config`).
    pub fn config_source(mut self, source: ConfigSource) -> Editor {
        self.config_source = source;
        self
    }

    /// Focus the editor as it's mounted, so keys go to it from the start. Off by
    /// default: in an app, where focus starts is the app's call.
    pub fn autofocus(mut self, on: bool) -> Editor {
        self.autofocus = on;
        self
    }

    /// The surface's accessible name (`bemtvi` by default).
    pub fn label(mut self, label: impl Into<String>) -> Editor {
        self.label = label.into();
        self
    }

    /// Start the IO thread on the session and make the surface, in the editor's own
    /// reactive scope: its tasks end with it, and so do its threads.
    fn mount(self, ui: &Ui) -> mitsuami::prelude::GpuSurface {
        let Editor {
            session,
            on_exit,
            handle,
            config,
            open_dir,
            config_source,
            autofocus,
            label,
            native_dialogs,
            on_event,
        } = self;
        let handle = handle.unwrap_or_default();
        let (proxy, events) = tokio::sync::mpsc::unbounded_channel();
        let (rpc_tx, rpc_rx) = std::sync::mpsc::channel::<Rpc>();
        let (reconnect_tx, reconnect_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fetch_tx, fetch_rx) = tokio::sync::mpsc::unbounded_channel();
        let (decode_tx, decode_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let remote = session.remote;
        // The window acts on a `:workspace` line itself, before the editor sees it, so a
        // `:workspace` the app disabled must not switch it either.
        let session_switching = session.launch.session_switching
            && !session
                .launch
                .disabled_commands
                .iter()
                .any(|name| bemtvi_core::ex_names::canonical_or_self(name) == "workspace");
        let thread = spawn_io(IoParts {
            initial: session,
            config_source,
            proxy,
            rpc_tx,
            reconnect_rx,
            fetch_rx,
            decode_rx,
            shutdown: shutdown.clone(),
        });
        // The first connection is up as soon as the thread runs (it's an in-process
        // duplex); only a thread that died first fails this.
        let rpc = rpc_rx
            .recv()
            .expect("bemtvi-gui: the client IO thread ended before it connected");
        let app = App::new(
            rpc,
            on_exit,
            config,
            open_dir,
            remote,
            reconnect_tx,
            fetch_tx,
            decode_tx,
        );
        {
            let mut app = app.borrow_mut();
            app.native_dialogs = native_dialogs;
            app.on_event = on_event;
            app.session_switching = session_switching;
        }
        handle.attach(&app, Io { shutdown, thread });
        {
            let (app, handle) = (app.clone(), handle.clone());
            on_cleanup(move || {
                handle.stop();
                // Its GPU resources go now, not whenever the last task lets go of it.
                if let Ok(mut app) = app.try_borrow_mut() {
                    app.renderer = None;
                }
            });
        }
        App::surface(app, events, ui, label, autofocus)
    }
}

impl View for Editor {
    fn build(self, ui: &Ui) -> NodeId {
        // A scope of the editor's own, under the one it's built in, so whatever drops
        // this view (a `Show` switching branches, a closed window) ends the editor too.
        let scope = Owner::new_child();
        let node = scope.with(|| self.mount(ui).build(ui));
        on_cleanup(move || scope.dispose());
        node
    }
}

/// The client IO thread of a mounted editor, and how to stop it.
struct Io {
    shutdown: Arc<tokio::sync::Notify>,
    thread: JoinHandle<bool>,
}

#[derive(Default)]
struct HandleState {
    app: Weak<RefCell<App>>,
    /// Whether an editor is mounted on this handle (until its scope ends).
    mounted: bool,
    io: Option<Io>,
    /// The IO threads of editors this handle was on before (unmounted, then mounted
    /// again), still to be joined.
    retired: Vec<Io>,
}

/// The app's handle on an [`Editor`]: pass a clone to [`Editor::handle`] and keep one.
/// UI-thread only, like the editor.
#[derive(Clone, Default)]
pub struct EditorHandle(Rc<RefCell<HandleState>>);

impl EditorHandle {
    pub fn new() -> EditorHandle {
        EditorHandle::default()
    }

    /// Whether the editor is mounted and hasn't quit.
    pub fn is_running(&self) -> bool {
        let state = self.0.borrow();
        state.mounted
            && state
                .app
                .upgrade()
                .is_some_and(|app| app.try_borrow().map_or(true, |app| !app.exited))
    }

    /// Ask the editor to quit, as the window's close button does: `:qa`, which refuses
    /// with `E37` while a buffer is unsaved, leaving the editor up. When it does quit,
    /// the editor's `on_exit` runs.
    pub fn request_quit(&self) -> Result<()> {
        self.command("qa")
    }

    /// Quit now, the way `:qall!` does, rather than letting the process be torn down
    /// mid-tick: the exit sequence runs (QuitPre/ExitPre/VimLeavePre/VimLeave, so
    /// plugins persist their state) and the server's clean-exit shada flush lands. The
    /// bang is deliberate — nobody is there to answer an E37 prompt — and so is the
    /// missing `w`: writing files the user never asked to write would be worse than
    /// losing the edits. For a process that was asked to stop (`bemtvi-gui` on a
    /// `kill`, SIGHUP or SIGINT).
    pub fn quit_now(&self) -> Result<()> {
        self.command("qall!")
    }

    /// Run Lua in the editor and resolve to what it returns (a chunk that raises
    /// resolves to the error). Values go into the chunk as text: the editor takes no
    /// arguments with it.
    pub fn exec_lua(
        &self,
        code: &str,
    ) -> impl std::future::Future<Output = Result<Value>> + 'static {
        let rpc = self.rpc();
        let code = Value::from(code);
        async move {
            rpc?.request("nvim_exec_lua", vec![code, Value::Array(Vec::new())])
                .await
        }
    }

    /// Open a file in the editor, as `:edit` does.
    pub fn open(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        // Escaped, so the command line reads the path as one file name: a `|` in it
        // would otherwise end the command and run what follows.
        let path = path.as_ref().to_string_lossy();
        self.command(&format!(
            "edit {}",
            bemtvi_core::ex_names::escape_file_arg(&path)
        ))
    }

    /// Give the editor the keyboard focus.
    pub fn focus(&self) -> Result<()> {
        let app = {
            let state = self.0.borrow();
            state.app.upgrade().filter(|_| state.mounted)
        }
        .ok_or_else(|| anyhow!("the editor isn't mounted"))?;
        let node = app.borrow().node.clone();
        // Through the UI it was built in, so this works from anywhere on the UI thread,
        // not only from inside a view or a task.
        match node.and_then(|(node, ui)| Some((node.get_untracked()?, ui))) {
            Some((id, ui)) => {
                ui.focus(id);
                Ok(())
            }
            None => Err(anyhow!("the editor has no surface yet")),
        }
    }

    /// Run an ex command in the editor, whatever mode it's in.
    pub fn command(&self, command: &str) -> Result<()> {
        self.rpc()?
            .notify("btv_command", vec![Value::from(command)]);
        Ok(())
    }

    /// Stop the editor's threads and wait for them: the IO thread drops the connection,
    /// so the server sees EOF and winds down, and every server thread is joined. Returns
    /// whether one of them panicked. The editor stops showing anything new.
    pub fn close(&self) -> bool {
        let ios: Vec<Io> = {
            let mut state = self.0.borrow_mut();
            let retired = std::mem::take(&mut state.retired);
            state.io.take().into_iter().chain(retired).collect()
        };
        let mut panicked = false;
        for io in ios {
            io.shutdown.notify_one();
            panicked |= io.thread.join().unwrap_or(true);
        }
        panicked
    }

    /// The editor was unmounted: its IO thread stops (without waiting for it).
    fn stop(&self) {
        let mut state = self.0.borrow_mut();
        state.mounted = false;
        if let Some(io) = &state.io {
            io.shutdown.notify_one();
        }
    }

    fn attach(&self, app: &Rc<RefCell<App>>, io: Io) {
        let mut state = self.0.borrow_mut();
        assert!(
            !state.mounted,
            "bemtvi-gui: an EditorHandle is on one mounted Editor at a time"
        );
        state.mounted = true;
        if let Some(previous) = state.io.take() {
            state.retired.push(previous);
        }
        state.app = Rc::downgrade(app);
        state.io = Some(io);
    }

    /// The live session's connection (it changes on a `:connect`).
    fn rpc(&self) -> Result<Rpc> {
        let app = {
            let state = self.0.borrow();
            state.app.upgrade().filter(|_| state.mounted)
        }
        .ok_or_else(|| anyhow!("the editor isn't mounted"))?;
        let app = app.borrow();
        if app.exited {
            return Err(anyhow!("the editor has quit"));
        }
        Ok(app.rpc.clone())
    }
}
