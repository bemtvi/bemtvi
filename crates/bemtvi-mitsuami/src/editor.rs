//! The editor as a mitsuami view ([`Editor`]) and the app's handle on it
//! ([`EditorHandle`]).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bemtvi_rpc::Rpc;
use bemtvi_server::ConfigSource;
use mitsuami::prelude::{on_cleanup, NodeId, Owner, Ui, View};
use rmpv::Value;

use bemtvi_gpu::{Client, ClientHandle, ClientOptions, GuiConfig, IoThread, Session};

use crate::app::App;

/// An editor, for a mitsuami view tree. Built on a running [`Session`] (see
/// [`SessionOptions::spawn`](crate::SessionOptions::spawn)), so a failed start — a daemon that refused the connection —
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
    /// server's listing of it (what `bemtvi-gui somedir` does). With no dialog to ask for
    /// (a daemon session, or [`Editor::native_dialogs`] off) it's `:e {dir}`: the listing.
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
        // The IO thread's wake, from any thread, for the UI-thread pump.
        let wake = Arc::new(tokio::sync::Notify::new());
        let waker = wake.clone();
        let (client, io) = Client::new(
            session,
            ClientOptions {
                config,
                open_dir,
                config_source,
                native_dialogs,
                option_chords: cfg!(target_os = "macos"),
            },
            Arc::new(move || waker.notify_one()),
        );
        let client_handle = client.handle();
        let app = App::new(client, on_exit, on_event);
        handle.attach(&app, client_handle, io);
        {
            let (app, handle) = (app.clone(), handle.clone());
            on_cleanup(move || {
                handle.stop();
                // Its GPU resources go now, not whenever the last task lets go of it.
                if let Ok(mut app) = app.try_borrow_mut() {
                    app.client.detach_surface();
                }
            });
        }
        App::surface(app, wake, ui, label, autofocus)
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

#[derive(Default)]
struct HandleState {
    app: Weak<RefCell<App>>,
    /// The mounted editor's, for its connection (it changes on a `:connect`).
    client: Option<ClientHandle>,
    /// Whether an editor is mounted on this handle (until its scope ends).
    mounted: bool,
    io: Option<IoThread>,
    /// The IO threads of editors this handle was on before (unmounted, then mounted
    /// again), still to be joined.
    retired: Vec<IoThread>,
    /// Whether one of the retired threads joined so far panicked, for `close` to say.
    panicked: bool,
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
        state.mounted && state.client.as_ref().is_some_and(|c| !c.exited())
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
        let client = self.client();
        let code = code.to_owned();
        async move { client?.exec_lua(&code).await }
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
        let (ios, mut panicked): (Vec<IoThread>, bool) = {
            let mut state = self.0.borrow_mut();
            let retired = std::mem::take(&mut state.retired);
            let ios = state.io.take().into_iter().chain(retired).collect();
            (ios, std::mem::take(&mut state.panicked))
        };
        for io in ios {
            panicked |= io.join();
        }
        panicked
    }

    /// How many IO threads of editors this handle was on before are still to be
    /// joined (by `close`, or by the next mount once they ended). For the embed tests.
    #[doc(hidden)]
    pub fn retired_threads(&self) -> usize {
        self.0.borrow().retired.len()
    }

    /// The editor was unmounted: its IO thread stops (without waiting for it).
    fn stop(&self) {
        let mut state = self.0.borrow_mut();
        state.mounted = false;
        if let Some(io) = &state.io {
            io.stop();
        }
    }

    fn attach(&self, app: &Rc<RefCell<App>>, client: ClientHandle, io: IoThread) {
        let mut state = self.0.borrow_mut();
        assert!(
            !state.mounted,
            "bemtvi-mitsuami: an EditorHandle is on one mounted Editor at a time"
        );
        state.mounted = true;
        if let Some(previous) = state.io.take() {
            state.retired.push(previous);
        }
        // Join the ones that ended: a thread nobody joins keeps its stack, and an app
        // may mount editors for as long as it runs before it closes the handle.
        let (ended, running) = std::mem::take(&mut state.retired)
            .into_iter()
            .partition::<Vec<_>, _>(IoThread::is_finished);
        state.retired = running;
        for io in ended {
            state.panicked |= io.join();
        }
        state.app = Rc::downgrade(app);
        state.client = Some(client);
        state.io = Some(io);
    }

    /// The mounted editor's handle; its connection follows a `:connect`.
    fn client(&self) -> Result<ClientHandle> {
        let state = self.0.borrow();
        match (&state.client, state.mounted) {
            (Some(client), true) => Ok(client.clone()),
            _ => Err(anyhow!("the editor isn't mounted")),
        }
    }

    /// The live session's connection, or why there's none (not mounted, quit).
    fn rpc(&self) -> Result<Rpc> {
        self.client()?.rpc()
    }
}
