//! The terminal-job engine — a per-buffer [`vt100`] emulator that turns a child's
//! raw PTY output into the screen the editor mirrors.
//!
//! This is the **emulation** half of `:terminal`, and it is deliberately
//! transport-agnostic: it never touches a process or the network, only bytes. The
//! editor tick feeds it the child's output ([`EditHost::terminal_feed`]); it
//! decodes the escape sequences into a screen grid and projects that grid into the
//! terminal buffer's mirrored lines + cursor (via [`Editor::terminal_update`]). The
//! per-cell colors are read straight off the live grid at redraw (Phase 4).
//!
//! Because it is pure CPU (no PTY, no async), it compiles to wasm and is shared by
//! both builds — the native server feeds it bytes from a local PTY, the browser
//! feeds it bytes streamed from the daemon. The byte transport that *gets* those
//! bytes is the part that differs (Phase 3 native / Phase 7 web). See
//! `docs/plans/2026-06-14-terminal-in-buffer.md`.

use nxvim_core::{BufferId, TerminalOp};

use crate::EditHost;

/// The `vt100` callback sink: captures the things the screen model itself doesn't
/// store but a real terminal must act on — the child's window title (OSC), and the
/// **replies** to status/identity queries (`vt100` is a screen *model*, so it never
/// answers them; we must, or apps like fzf that send `\e[6n` stall waiting). The
/// emulator reads both back via [`vt100::Parser::callbacks_mut`] after each `process`.
#[derive(Default)]
struct TermSink {
    title: Option<String>,
    /// Bytes to write back to the child (cursor-position / device-attributes reports),
    /// drained and sent to the pty after the feed.
    replies: Vec<u8>,
}

impl vt100::Callbacks for TermSink {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    /// Answer the terminal queries a screen model can't — the same replies a real
    /// terminal emits automatically. Without these, inline TUIs (fzf, …) that probe
    /// the cursor position before drawing block until the next keystroke.
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (c, i1) {
            // Device Status Report.
            ('n', None) => match p0 {
                5 => self.replies.extend_from_slice(b"\x1b[0n"), // "terminal OK"
                6 => {
                    // Cursor Position Report — 1-based row;col.
                    let (row, col) = screen.cursor_position();
                    self.replies
                        .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                _ => {}
            },
            // Primary Device Attributes: identify as a VT102.
            ('c', None) => self.replies.extend_from_slice(b"\x1b[?6c"),
            // Secondary Device Attributes.
            ('c', Some(b'>')) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }
}

/// A terminal buffer's vt100 emulator: the escape-sequence parser (which owns the
/// screen grid) plus the last size it was projected at.
pub(crate) struct TermEmu {
    /// The vt100 parser + screen grid. Fed the child's PTY bytes; queried for the
    /// row text, cursor, per-cell colors, and (via its [`TermSink`]) window title +
    /// query replies.
    parser: vt100::Parser<TermSink>,
    /// The `(rows, cols)` the emulator was last sized to, so a redraw-time resize
    /// only re-sizes (and reprojects) when the window's text area actually changed.
    last_size: (u16, u16),
}

impl TermEmu {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = (rows.max(1), cols.max(1));
        TermEmu {
            // Scrollback is 0 for now — the visible screen only. Phase 6 raises it
            // and projects the scrolled-off history into the buffer.
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, TermSink::default()),
            last_size: (rows, cols),
        }
    }
}

impl EditHost {
    /// Create (or reset) the vt100 emulator for terminal buffer `buf`, sized
    /// `rows`×`cols`, and project its initial (blank) screen so the buffer shows the
    /// right number of rows immediately. Called when a `:terminal` spawns its PTY.
    pub fn terminal_open_emu(&mut self, buf: BufferId, rows: u16, cols: u16) {
        self.terminals.insert(buf, TermEmu::new(rows, cols));
        self.terminal_project(buf);
    }

    /// Feed `bytes` of the child's PTY output into `buf`'s emulator, then reproject
    /// the screen into the buffer. Any status/identity queries the child emitted are
    /// answered by writing the reply bytes back to it (the same path a keystroke
    /// takes), so apps that probe the terminal before drawing don't stall. A no-op if
    /// `buf` has no live emulator.
    pub fn terminal_feed(&mut self, buf: BufferId, bytes: &[u8]) {
        let replies = match self.terminals.get_mut(&buf) {
            Some(emu) => {
                emu.parser.process(bytes);
                std::mem::take(&mut emu.parser.callbacks_mut().replies)
            }
            None => return,
        };
        if !replies.is_empty() {
            self.editor.terminal_send(buf, replies);
        }
        self.terminal_project(buf);
    }

    /// Resize `buf`'s emulator to `rows`×`cols`, reprojecting on a real change.
    /// Returns whether the size changed, so the caller can also resize the PTY (the
    /// child needs the new winsize to reflow). A no-op (returns `false`) if `buf`
    /// has no live emulator or the size is unchanged.
    pub fn terminal_resize(&mut self, buf: BufferId, rows: u16, cols: u16) -> bool {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let Some(emu) = self.terminals.get_mut(&buf) else {
            return false;
        };
        if emu.last_size == (rows, cols) {
            return false;
        }
        emu.parser.screen_mut().set_size(rows, cols);
        emu.last_size = (rows, cols);
        self.terminal_project(buf);
        true
    }

    /// Drop `buf`'s emulator — the terminal closed, or the buffer was wiped.
    pub fn terminal_remove(&mut self, buf: BufferId) {
        self.terminals.remove(&buf);
    }

    /// Project `buf`'s emulator screen into the buffer's mirrored lines + cursor.
    /// The grid's per-cell colors are read separately at redraw (Phase 4); here we
    /// only push the text and cursor through the pure-core inbound API.
    fn terminal_project(&mut self, buf: BufferId) {
        let (lines, cursor_row, cursor_col, title) = {
            let Some(emu) = self.terminals.get(&buf) else {
                return;
            };
            let screen = emu.parser.screen();
            let (_rows, cols) = screen.size();
            let lines: Vec<String> = screen.rows(0, cols).collect();
            let (cy, cx) = screen.cursor_position();
            let title = emu.parser.callbacks().title.clone();
            (lines, cy as usize, cx as usize, title)
        };
        self.editor
            .terminal_update(buf, &lines, cursor_row, cursor_col);
        if let Some(title) = title {
            self.editor.terminal_set_title(buf, &title);
        }
    }
}

/// Drain core's terminal ops (`Editor::take_pending_terminal`) and route them to the
/// transport — the bridge between the pure-core queue and the PTY. Native spawns /
/// writes / kills a real local PTY; the wasm build (until Phase 7 wires the daemon
/// leg) has no transport, so it fails the open *loud* rather than silently dropping
/// it. Called from the settle path each tick.
#[cfg(feature = "native")]
impl EditHost {
    pub(crate) fn dispatch_terminal_ops(&mut self, ops: Vec<TerminalOp>) {
        for op in ops {
            match op {
                TerminalOp::Open {
                    buf,
                    argv,
                    cwd,
                    rows,
                    cols,
                } => {
                    // Build the emulator first so the very next redraw projects the
                    // (blank) screen at the right size, then spawn the PTY behind it.
                    self.terminal_open_emu(buf, rows, cols);
                    self.fx.terminal_command(native::TermCommand::Open {
                        buf,
                        argv,
                        cwd,
                        rows,
                        cols,
                    });
                }
                TerminalOp::Send { buf, bytes } => {
                    self.fx
                        .terminal_command(native::TermCommand::Write { buf, bytes });
                }
                TerminalOp::Kill { buf } => {
                    self.terminal_remove(buf);
                    self.fx.terminal_command(native::TermCommand::Kill { buf });
                }
            }
        }
    }

    /// Keep the current terminal's PTY winsize matching its window text area. Called
    /// each redraw: when the focused window shows a terminal and its text rect
    /// changed (a UI resize, a `<C-w>` resize), reflow the emulator and resize the
    /// child's PTY so it re-lays-out. (Terminals in unfocused splits are resized when
    /// next focused — a follow-up.)
    pub(crate) fn sync_terminal_sizes(&mut self) {
        let buf = self.editor.current_buffer_id();
        if !self.terminals.contains_key(&buf) {
            return;
        }
        let (rows, cols) = self.editor.current_text_area();
        if self.terminal_resize(buf, rows, cols) {
            self.fx
                .terminal_command(native::TermCommand::Resize { buf, rows, cols });
        }
    }
}

/// No terminal transport in the serverless browser build yet — Phase 7 wires the
/// daemon PTY over WebTransport. Until then an open fails *loud* (no silent stub).
#[cfg(not(feature = "native"))]
impl EditHost {
    pub(crate) fn dispatch_terminal_ops(&mut self, ops: Vec<TerminalOp>) {
        for op in ops {
            if let TerminalOp::Open { buf, .. } = op {
                self.editor.terminal_closed(buf, -1);
                self.editor
                    .echo("E: :terminal requires a daemon connection in this build".to_string());
            }
            // Send / Kill for a terminal that never opened are no-ops.
        }
    }
}

/// The native PTY transport: a `Send` actor (modeled on
/// [`EventLoop`](crate::evloop::EventLoop)) that owns the real local pseudo-terminals
/// and streams their output back to the editor thread. The editor tick fires
/// fire-and-forget [`TermCommand`]s at it; their output / exit return as
/// [`TermEvent`]s on the run loop's `select!`. The vt100 emulation lives in
/// [`EditHost`] (above), so this layer only moves bytes.
#[cfg(feature = "native")]
pub(crate) mod native {
    use std::collections::HashMap;
    use std::io::{Read, Write};

    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    use nxvim_core::BufferId;

    /// A command from the editor tick to the terminal actor. Fire-and-forget, like
    /// [`LoopCommand`](crate::evloop::LoopCommand): the editor never awaits a reply.
    pub enum TermCommand {
        /// Spawn a PTY child for `buf`, sized `rows`×`cols`, running `argv` in `cwd`
        /// (empty `argv` ⇒ the default shell, `cwd` `None` ⇒ inherit).
        Open {
            buf: BufferId,
            argv: Vec<String>,
            cwd: Option<String>,
            rows: u16,
            cols: u16,
        },
        /// Write input bytes to `buf`'s PTY (a forwarded keystroke / paste).
        Write { buf: BufferId, bytes: Vec<u8> },
        /// Resize `buf`'s PTY so the child re-lays-out (window resize).
        Resize { buf: BufferId, rows: u16, cols: u16 },
        /// Kill `buf`'s child and forget the session.
        Kill { buf: BufferId },
    }

    /// An event from the terminal actor back to the editor thread, delivered to the
    /// run loop's `select!`. The matching [`EditHost`](crate::EditHost) handler feeds
    /// the bytes to the buffer's emulator / records the exit on the one server thread.
    #[derive(Debug)]
    pub enum TermEvent {
        /// `buf`'s child wrote output (raw PTY bytes — fed to the vt100 emulator).
        Data { buf: BufferId, bytes: Vec<u8> },
        /// `buf`'s child exited with `code` (`-1` on a spawn failure or a kill).
        Exit { buf: BufferId, code: i32 },
    }

    /// Handle the server holds to drive the terminal actor. Cheap to construct; the
    /// actor task is spawned lazily on the first [`send`](TerminalManager::send), so a
    /// session that never opens a terminal spawns nothing (the [`EventLoop`] pattern).
    ///
    /// [`EventLoop`]: crate::evloop::EventLoop
    pub struct TerminalManager {
        cmd_tx: UnboundedSender<TermCommand>,
        start: Option<(UnboundedReceiver<TermCommand>, UnboundedSender<TermEvent>)>,
        started: bool,
    }

    impl TerminalManager {
        /// Create the manager and the receiver the run loop selects on. No task is
        /// spawned until the first [`send`](Self::send).
        pub fn new() -> (TerminalManager, UnboundedReceiver<TermEvent>) {
            let (cmd_tx, cmd_rx) = unbounded_channel();
            let (event_tx, event_rx) = unbounded_channel();
            let mgr = TerminalManager {
                cmd_tx,
                start: Some((cmd_rx, event_tx)),
                started: false,
            };
            (mgr, event_rx)
        }

        fn ensure_started(&mut self) {
            if self.started {
                return;
            }
            if let Some((cmd_rx, event_tx)) = self.start.take() {
                tokio::spawn(run_terminal_actor(cmd_rx, event_tx));
                self.started = true;
            }
        }

        /// Fire-and-forget a command at the actor, starting it on first use.
        pub fn send(&mut self, cmd: TermCommand) {
            self.ensure_started();
            let _ = self.cmd_tx.send(cmd);
        }
    }

    /// A live PTY: the master's writer (input), the master itself (resize), and a
    /// killer cloned off the child (the child itself is moved into its wait thread).
    struct Session {
        writer: Box<dyn Write + Send>,
        master: Box<dyn portable_pty::MasterPty + Send>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    }

    /// The actor's run loop: own the live PTYs, service commands, and let each PTY's
    /// reader / waiter threads send output / exit back. Ends when the server drops the
    /// command sender (shutdown).
    async fn run_terminal_actor(
        mut cmd_rx: UnboundedReceiver<TermCommand>,
        event_tx: UnboundedSender<TermEvent>,
    ) {
        let mut sessions: HashMap<BufferId, Session> = HashMap::new();
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TermCommand::Open {
                    buf,
                    argv,
                    cwd,
                    rows,
                    cols,
                } => {
                    match open_pty(buf, argv, cwd, rows, cols, &event_tx) {
                        Ok(session) => {
                            // Re-opening an id replaces any prior session (its child
                            // is dropped → killed).
                            sessions.insert(buf, session);
                        }
                        Err(e) => {
                            // Surface the failure in the buffer, then end the job —
                            // never a silent drop.
                            let _ = event_tx.send(TermEvent::Data {
                                buf,
                                bytes: format!("nxvim: {e}\r\n").into_bytes(),
                            });
                            let _ = event_tx.send(TermEvent::Exit { buf, code: -1 });
                        }
                    }
                }
                TermCommand::Write { buf, bytes } => {
                    if let Some(s) = sessions.get_mut(&buf) {
                        let _ = s.writer.write_all(&bytes);
                        let _ = s.writer.flush();
                    }
                }
                TermCommand::Resize { buf, rows, cols } => {
                    if let Some(s) = sessions.get(&buf) {
                        let _ = s.master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                TermCommand::Kill { buf } => {
                    if let Some(mut s) = sessions.remove(&buf) {
                        let _ = s.killer.kill();
                    }
                }
            }
        }
    }

    /// Spawn one PTY child for `buf` and wire its I/O: a reader thread streams output
    /// as [`TermEvent::Data`], a waiter thread reports the exit as [`TermEvent::Exit`].
    /// Both are plain OS threads (portable-pty's reader/wait are blocking); they end
    /// when the child exits (EOF on the master) or the event channel closes.
    fn open_pty(
        buf: BufferId,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
        event_tx: &UnboundedSender<TermEvent>,
    ) -> anyhow::Result<Session> {
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut builder = match argv.split_first() {
            Some((program, args)) => {
                let mut b = CommandBuilder::new(program);
                b.args(args);
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(dir) = cwd {
            builder.cwd(dir);
        }
        let child = pair.slave.spawn_command(builder)?;
        // Drop the slave handle so the child is the only writer to the pty — once it
        // exits, the master read returns EOF and the reader thread ends.
        drop(pair.slave);
        let killer = child.clone_killer();
        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let data_tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if data_tx
                            .send(TermEvent::Data {
                                buf,
                                bytes: chunk[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let exit_tx = event_tx.clone();
        let mut child = child;
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = exit_tx.send(TermEvent::Exit { buf, code });
        });

        Ok(Session {
            writer,
            master: pair.master,
            killer,
        })
    }
}
