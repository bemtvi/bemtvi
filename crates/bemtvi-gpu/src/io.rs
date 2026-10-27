//! The client IO thread: the RPC to the current session, each `redraw` decoded into a
//! [`View`] for the UI thread, the sessions `:connect` / `:workspace` swap in, and the
//! joins of every server thread at the end.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bemtvi_rpc::{connect, Incoming, Rpc};
use bemtvi_view::View;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{remote, session, DecodeReq, ImageFetch, Session};

/// Events the IO thread sends the UI thread.
pub enum UserEvent {
    /// A decoded `redraw`: replace the view and repaint.
    Redraw(Box<View>),
    /// An `bemtvi_image_read` reply: a remote preview's fetched bytes (or a read error),
    /// to hand to the image store and repaint. Carries the version the fetch was for.
    ImageBytes {
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    },
    /// An off-thread preview decode's outcome (or failure), to hand to the image
    /// store and repaint. Carries the version the decode was requested at.
    ImageDecoded {
        path: String,
        version: (u64, u64),
        decoded: Option<image::DynamicImage>,
    },
    /// A `:connect` brought up a new (daemon or local) session: swap the App's live
    /// RPC handle to it, mark whether it is remote, and re-attach the UI.
    Connected {
        rpc: Box<Rpc>,
        /// Whether the new session is an edit-host (daemon) one — drives the local
        /// file-dialog suppression (see [`dialog_action`](crate::dialog_action)).
        remote: bool,
    },
    /// The server asked the UI to exit (`bemtvi_exit`), or the connection closed.
    Exit,
    /// `btv.host.send(name, payload)`: a message for the app embedding the editor.
    Host { name: String, payload: Value },
    /// A submitted `:` line asked for a file dialog (`btv_file_dialog`): the open one
    /// (`save` false, in `start_dir` when it names one) or the save one, then `base` with
    /// the file picked.
    FileDialog {
        save: bool,
        base: String,
        start_dir: Option<PathBuf>,
    },
}

/// The IO thread's way to the UI thread: an event on the channel, then the host's
/// waker, so a host whose UI thread doesn't poll the channel knows to drain it.
#[derive(Clone)]
pub struct Proxy {
    tx: tokio::sync::mpsc::UnboundedSender<UserEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Proxy {
    pub fn new(
        tx: tokio::sync::mpsc::UnboundedSender<UserEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Proxy {
        Proxy { tx, wake }
    }

    /// Send `event` and wake the UI thread; an `Err` when the UI side is gone.
    #[allow(clippy::result_large_err)]
    pub fn send(
        &self,
        event: UserEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<UserEvent>> {
        self.tx.send(event)?;
        (self.wake)();
        Ok(())
    }
}

/// What the client IO thread runs on: the session it starts on, and its ends of the
/// channels to the UI thread. See [`spawn_io`].
pub struct IoParts {
    pub initial: Session,
    pub config_source: bemtvi_server::ConfigSource,
    /// Events to the UI thread, each followed by a call to its waker.
    pub proxy: Proxy,
    /// Hands the first connection's `Rpc` back to the UI thread, so it can fire input
    /// synchronously.
    pub rpc_tx: std::sync::mpsc::Sender<Rpc>,
    /// The image store requests remote preview bytes here; the IO thread fulfils each
    /// over `bemtvi_image_read` on the *current* session and posts the bytes back as
    /// `UserEvent::ImageBytes`.
    pub fetch_rx: UnboundedReceiver<ImageFetch>,
    /// Preview decodes, the same shape: the (synchronous) paint can only enqueue a
    /// decode request — local previews carry just the path, remote ones their fetched
    /// bytes — and the IO thread runs the disk read + full-res decode on a blocking
    /// task, never on the UI thread, posting the outcome back as
    /// `UserEvent::ImageDecoded`.
    pub decode_rx: UnboundedReceiver<DecodeReq>,
    /// Notified when the UI is done with the editor: the IO thread then drops the RPC
    /// connection. The IO thread (not the UI) owns the stream, so unless it closes
    /// that, the server never sees EOF, never winds down, and the join hangs forever —
    /// the close-button freeze.
    pub shutdown: Arc<tokio::sync::Notify>,
}

/// Start the client IO thread. The host's toolkit owns the UI thread, so the RPC runs here on a
/// current-thread tokio runtime: it drives [`bemtvi_rpc::connect`] for the current
/// session, decodes each `redraw` into a [`View`] for the UI thread, builds the new
/// session a `:connect`/`:workspace` asks for, and joins every server thread at the end.
/// Its result is whether a server thread panicked.
pub fn spawn_io(parts: IoParts) -> std::thread::JoinHandle<bool> {
    let IoParts {
        initial,
        config_source,
        proxy,
        rpc_tx,
        mut fetch_rx,
        mut decode_rx,
        shutdown: io_shutdown,
    } = parts;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build client IO runtime");
        let servers = runtime.block_on(async move {
        // The current session's transport + server thread, plus the retired servers
        // of earlier `:connect`s (each winds down on EOF; joined at teardown).
        let mut stream = initial.stream;
        // How the sessions here start: a `:connect`/`:workspace` starts the next one
        // the way the first was started.
        let launch = initial.launch;
        let mut current_handle = Some(initial.handle);
        let mut next_remote = initial.remote;
        let mut retired: Vec<std::thread::JoinHandle<anyhow::Result<()>>> = Vec::new();
        // A finished `:connect` build delivers its new session here, so the handshake
        // (run off the UI thread on the blocking pool) doesn't stall the current
        // session's redraws while it's in flight.
        // Each carries the command label (`:connect` / `:workspace`) so a build
        // failure is reported against the command the user actually typed.
        let (built_tx, mut built_rx) =
            tokio::sync::mpsc::unbounded_channel::<(&'static str, Result<Session>)>();
        let mut first = true;

        'session: loop {
            let (reader, writer) = tokio::io::split(stream);
            let (rpc, mut incoming) = connect(reader, writer);
            // Hand the connection to the App: the first synchronously (so `App::new`
            // has an `Rpc`); later ones as `Connected`, which swaps the live handle
            // and re-attaches. Dropping the *old* session's `rpc`/`incoming` on the
            // next iteration — together with the App swapping its clone — winds the
            // previous connection (and its server) down.
            if first {
                let _ = rpc_tx.send(rpc.clone());
                first = false;
            } else {
                let event = UserEvent::Connected {
                    rpc: Box::new(rpc.clone()),
                    remote: next_remote,
                };
                if proxy.send(event).is_err() {
                    break 'session; // event loop gone
                }
            }

            // Block until a `:connect` build succeeds (the only value-carrying
            // break), or `break 'session` on disconnect / exit / shutdown.
            let new_session: Session = loop {
                tokio::select! {
                    message = incoming.recv() => match message {
                        Some(Incoming::Notification { method, params }) => match method.as_str() {
                            "redraw" => {
                                let view = View::from_redraw(&params);
                                if proxy.send(UserEvent::Redraw(Box::new(view))).is_err() {
                                    break 'session; // event loop gone
                                }
                            }
                            "bemtvi_exit" => {
                                let _ = proxy.send(UserEvent::Exit);
                                break 'session;
                            }
                            // `btv.host.send(name, payload)`: for the app embedding the
                            // editor, through the UI thread (where its handler runs).
                            "bemtvi_host" => {
                                let mut params = params.into_iter();
                                let name = params.next();
                                let payload = params.next().unwrap_or(Value::Nil);
                                match name.as_ref().and_then(Value::as_str) {
                                    Some(name) => {
                                        let name = name.to_owned();
                                        if proxy.send(UserEvent::Host { name, payload }).is_err() {
                                            break 'session; // event loop gone
                                        }
                                    }
                                    None => report_session_error(
                                        &rpc,
                                        "btv.host.send",
                                        &anyhow::anyhow!("a message with no name: {name:?}"),
                                    ),
                                }
                            }
                            // A submitted `:e`, `:eo`, `:wo`, … asking for a file dialog
                            // (`bemtvi_core::dialog`): `[kind, base, start_dir]`.
                            "btv_file_dialog" => {
                                let kind = params.first().and_then(Value::as_str);
                                let base = params.get(1).and_then(Value::as_str);
                                let start_dir = params
                                    .get(2)
                                    .and_then(Value::as_str)
                                    .map(PathBuf::from);
                                match (kind, base) {
                                    (Some(kind @ ("open" | "save")), Some(base)) => {
                                        let event = UserEvent::FileDialog {
                                            save: kind == "save",
                                            base: base.to_owned(),
                                            start_dir,
                                        };
                                        if proxy.send(event).is_err() {
                                            break 'session; // event loop gone
                                        }
                                    }
                                    _ => report_session_error(
                                        &rpc,
                                        "btv_file_dialog",
                                        &anyhow::anyhow!("a malformed request: {params:?}"),
                                    ),
                                }
                            }
                            // `:workspace [dir]` ran (the GUI's command, see
                            // `session::CLIENT_INIT_LUA`): swap onto a local session rooted
                            // there, built off the UI thread like `:connect`. The editor
                            // resolved `dir` against its working directory.
                            "btv_workspace" if !launch.session_switching => {
                                report_session_error(&rpc, ":workspace", &switching_off());
                            }
                            "btv_workspace" => match params.first().and_then(Value::as_str) {
                                Some(dir) => {
                                    let tx = built_tx.clone();
                                    let launch = launch.clone();
                                    let dir = PathBuf::from(dir);
                                    tokio::task::spawn_blocking(move || {
                                        let built = session::spawn_workspace_session(
                                            dir,
                                            config_source,
                                            launch,
                                        );
                                        let _ = tx.send((":workspace", built));
                                    });
                                }
                                None => report_session_error(
                                    &rpc,
                                    ":workspace",
                                    &anyhow::anyhow!("btv_workspace: missing dir"),
                                ),
                            },
                            // `btv.session.reconnect(spec)` from inside the VM (§B): a
                            // plugin initiates the reload. Parse the wire spec and build
                            // the new session off the UI thread (like `:connect`), then
                            // `built_rx` swaps onto it. A bad spec / failed build is
                            // reported and leaves the current session intact.
                            "btv_session_reconnect" if !launch.session_switching => {
                                report_session_error(&rpc, ":reconnect", &switching_off());
                            }
                            "btv_session_reconnect" => {
                                match params.first().ok_or_else(|| {
                                    anyhow::anyhow!("btv_session_reconnect: missing spec")
                                }).and_then(bemtvi_server::ReconnectSpec::from_value) {
                                    Ok(spec) => {
                                        let tx = built_tx.clone();
                                        let launch = launch.clone();
                                        tokio::task::spawn_blocking(move || {
                                            let built = session::spawn_session_from_spec(spec, launch);
                                            let _ = tx.send((":reconnect", built));
                                        });
                                    }
                                    Err(err) => report_session_error(&rpc, ":reconnect", &err),
                                }
                            }
                            // `:connect <url>` with no matching connect-provider (§C): the
                            // VM hands the raw URL back for the GUI's built-in direct dial.
                            // Parse it into an ssh/quic target and build off the UI thread
                            // (the ssh handshake and its askpass dialog can take seconds);
                            // the swap arrives on `built_rx`. An unparseable URL is
                            // reported, not swallowed.
                            "btv_connect_fallback" if !launch.session_switching => {
                                report_session_error(&rpc, ":connect", &switching_off());
                            }
                            "btv_connect_fallback" => {
                                match params.first().and_then(Value::as_str) {
                                    Some(url) => match remote::connect_target(url) {
                                        Some(target) => {
                                            let tx = built_tx.clone();
                                            let launch = launch.clone();
                                            tokio::task::spawn_blocking(move || {
                                                let file = target.embedded_file();
                                                let built = session::spawn_session_with(
                                                    Some(target),
                                                    file,
                                                    config_source,
                                                    launch,
                                                );
                                                let _ = tx.send((":connect", built));
                                            });
                                        }
                                        None => report_session_error(
                                            &rpc,
                                            ":connect",
                                            &anyhow::anyhow!(
                                                "not a connect target: {url:?} (expected bemtvi://… or [user@]host[:port][/file])"
                                            ),
                                        ),
                                    },
                                    None => report_session_error(
                                        &rpc,
                                        ":connect",
                                        &anyhow::anyhow!("btv_connect_fallback: missing url"),
                                    ),
                                }
                            }
                            _ => {}
                        },
                        // Answer server-initiated requests with nil, like the TUI.
                        Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                        None => break 'session, // connection closed → exit UI
                    },
                    // A `:connect`/`:workspace` build finished. On success, restart the
                    // session on it; on failure, keep the current session and report why.
                    built = built_rx.recv() => match built {
                        Some((_, Ok(session))) => break session,
                        Some((label, Err(err))) => report_session_error(&rpc, label, &err),
                        None => {} // unreachable: `built_tx` is held above
                    },
                    // The render thread needs a remote preview's bytes. Fetch them
                    // over `bemtvi_image_read` on the *current* session (a spawned
                    // task, so a slow daemon read doesn't stall redraws) and post the
                    // reply back to the UI thread, which hands it to the image store.
                    // `None` (App, the only sender, is gone) just falls through —
                    // teardown is handled by the other arms.
                    fetch = fetch_rx.recv() => if let Some(ImageFetch { path, version }) = fetch {
                        let rpc = rpc.clone();
                        let proxy = proxy.clone();
                        tokio::spawn(async move {
                            let result = bemtvi_view::images::image_read_reply(
                                rpc.request(
                                    "bemtvi_image_read",
                                    vec![Value::from(path.as_str())],
                                )
                                .await,
                            );
                            let _ = proxy.send(UserEvent::ImageBytes {
                                path,
                                version,
                                result,
                            });
                        });
                    },
                    // A preview needs decoding: run the disk read + full-res decode
                    // on a blocking task (a slow disk or a huge image must never
                    // stall repaints) and post the outcome back as
                    // `UserEvent::ImageDecoded`.
                    decode = decode_rx.recv() => if let Some(req) = decode {
                        let proxy = proxy.clone();
                        tokio::task::spawn_blocking(move || {
                            let (path, version, decoded) =
                                req.decode(bemtvi_view::images::MAX_EDGE);
                            let _ = proxy.send(UserEvent::ImageDecoded {
                                path,
                                version,
                                decoded,
                            });
                        });
                    },
                    // The UI exited: stop, so dropping the runtime below closes the
                    // connection and the server winds down.
                    _ = io_shutdown.notified() => break 'session,
                }
            };

            // Swap in the new session: retire the old server (it winds down when this
            // iteration's `rpc`/`incoming` drop on `continue`) and re-loop on the new
            // transport, which re-attaches the UI via `Connected`.
            if let Some(handle) = current_handle.take() {
                retired.push(handle);
            }
            current_handle = Some(new_session.handle);
            next_remote = new_session.remote;
            stream = new_session.stream;
            continue 'session;
        }

        // Tell the UI to exit if it hasn't already.
        let _ = proxy.send(UserEvent::Exit);
        current_handle.into_iter().chain(retired).collect::<Vec<_>>()
    });
        // Join the servers outside `block_on`, with the runtime gone. Blocked inside it, the
        // runtime never runs its tasks again — they own the connections — and a server that
        // hadn't quit by itself (an unmounted editor's) never winds down, so the join hangs.
        // Shutting the runtime down drops those tasks: each server sees EOF. In the
        // background, because a plain drop also waits for the blocking pool: a
        // `:connect` still dialing (a dead host, an ssh handshake) would hold the close
        // until the dial gave up. The dial ends on its own, and its session, finding
        // nobody to take it, is dropped and winds down.
        runtime.shutdown_background();
        // Join every server thread (current + retired), surfacing a panic so a crashed
        // server is a non-zero exit, not a silent clean quit.
        let mut panicked = false;
        for handle in servers {
            match handle.join() {
                Err(_) => panicked = true,
                // A server error (not a panic): report it like the old in-thread
                // eprintln did, but keep the exit clean.
                Ok(Err(e)) => eprintln!("bemtvi-gui: server error: {e}"),
                Ok(Ok(())) => {}
            }
        }
        panicked
    })
}

/// Why a session switch was refused in an editor whose launch doesn't allow them.
fn switching_off() -> anyhow::Error {
    anyhow::anyhow!("this editor can't switch sessions (its host turned that off)")
}

/// Report a session-switch failure in the GUI message line via the *current* session —
/// the new one never came up. `label` is the command that failed (`:connect` — bad host,
/// refused auth, a malformed `bemtvi://` URI; or `:workspace` — a missing / non-directory
/// path). The error chain is flattened to one line, and shown as an error (see
/// [`notify_editor`]).
pub fn report_session_error(rpc: &Rpc, label: &str, err: &anyhow::Error) {
    let line = format!("{label} failed: {err:#}").replace('\n', "; ");
    notify_editor(rpc, &line, "ERROR");
}

/// Show `message` in the editor through `btv.notify` at `level` (a `btv.log.levels`
/// name), as a Lua chunk with the message in it as a literal ([`lua_string`]):
/// `nvim_exec_lua` takes no arguments. Not an `:echoerr '…'` — a command line splits
/// at a `|`, so a path or URL holding one would end the message there.
pub(crate) fn notify_editor(rpc: &Rpc, message: &str, level: &'static str) {
    rpc.notify(
        "nvim_exec_lua",
        vec![
            Value::from(format!(
                "btv.notify({}, btv.log.levels.{level})",
                lua_string(message)
            )),
            Value::Array(Vec::new()),
        ],
    );
}

/// `s` as a Lua string literal: every byte but plain ASCII letters, digits and a few
/// harmless marks goes in as a three-digit `\ddd` escape (always three, so a digit
/// after it can't join it), so nothing in `s` can end the literal.
pub(crate) fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b" ,.:-_()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("\\{b:03}"));
        }
    }
    out.push('"');
    out
}
