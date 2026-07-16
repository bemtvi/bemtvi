//! The terminal UI client.
//!
//! A thin RPC client that owns no editor state. It attaches to the server,
//! sends keystrokes as vim key-notation (`nx_input`), and renders the
//! server's [`View`](nxvim_view::View) using **ratatui-native widgets, one per
//! region**: the text area, the status line, and the command line are laid out
//! with a ratatui `Layout` and drawn as separate widgets. There is no neovim UI
//! protocol and no custom cell renderer.
//!
//! The client owns the screen layout: it reserves one row for the global command
//! line (each window draws its own status line inside its rect) and tells the
//! server only how tall the *windows area* is, so scrolling stays correct. Input
//! and redraw are multiplexed with `tokio::select!`.
//!
//! The semantic-view decode/input layer is frontend-neutral and lives in the
//! [`nxvim_view`] crate (it mirrors the server's view, parses each `redraw`, and
//! holds the msgpack accessors). The TUI-specific work is split across submodules:
//! [`anim`] is the scroll-animation state machine, [`render`] paints the frame,
//! [`images`] renders `'imagepreview'` pictures via ratatui-image, and [`keys`]
//! encodes key events. This module keeps only the event loop and transport.

mod anim;
mod images;
mod keys;
mod render;

pub use keys::encode_key;
pub use render::{
    cursor_style, paint, paint_doc_scrolled, paint_with_cursor, pmenu_doc_geometry, pmenu_geometry,
    ScrollHarness,
};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_view::{encode_paste, View};
use rmpv::Value;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

use anim::{arm_animation, Animation};
use ratatui::DefaultTerminal;
use render::render;

/// Rows reserved at the bottom for the global command/message line. Each window
/// now draws its own status line inside its rect, so only the command line is
/// reserved here; the height the client reports is the windows-area height.
const CHROME_ROWS: u16 = 1;

/// How often a held-at-the-edge mouse drag re-sends itself to keep the buffer
/// auto-scrolling (≈25 lines/sec). The terminal only reports a drag on pointer
/// motion, so without this repeat a selection dragged to the edge and held still
/// would stop scrolling; this paces the continuous scroll the server does a line
/// at a time per drag event.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);

/// Whether a drag at windows-area `row` (global, 0-based) sits in the top/bottom
/// edge band that arms continuous auto-scroll, for a terminal `height` rows tall.
/// The top row (above a tabline, or the first text row) and the bottom windows
/// rows (the status line and below, where a drag has crossed past the text body)
/// qualify; the server decides whether that actually scrolls the focused window.
fn in_scroll_zone(row: u16, height: u16) -> bool {
    // Saturate: `row` comes straight off the terminal's wire, so a bogus value
    // near `u16::MAX` must not overflow (a debug-build panic in the input loop).
    row == 0 || row.saturating_add(2) >= height
}

/// RAII guard for terminal mouse capture: enables mouse reporting on creation
/// and **disables it on drop**, including when the event loop unwinds on a
/// panic. ratatui's panic hook restores raw mode and the alternate screen but
/// does *not* touch mouse mode, so without a drop guard a panic would leave the
/// terminal reporting mouse events — spraying the user's shell with escape
/// codes on every click and move. The guard fires on the normal return path and
/// the panic path alike. Generic over the writer so it can be tested against an
/// in-memory sink; production uses `std::io::stdout()`.
pub struct MouseCapture<W: Write> {
    out: W,
}

impl<W: Write> MouseCapture<W> {
    /// Enable mouse capture on `out`; the returned guard disables it on drop.
    pub fn enable(mut out: W) -> Self {
        let _ = crossterm::execute!(out, EnableMouseCapture);
        Self { out }
    }
}

impl<W: Write> Drop for MouseCapture<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, DisableMouseCapture);
    }
}

/// RAII guard that restores the terminal's **default cursor shape on drop**. The
/// client swaps the cursor to a thin bar in insert mode (see
/// [`cursor_style`](render::cursor_style)); this guarantees the user's configured
/// cursor comes back when the client leaves — on the normal return path and on a
/// panic-unwind alike. Like [`MouseCapture`], it exists because ratatui's panic
/// hook restores raw mode and the alternate screen but *not* the cursor shape, so
/// without it a panic in insert mode would leave the user's shell with a bar
/// cursor. Generic over the writer for testing; production uses `std::io::stdout()`.
pub struct CursorStyleGuard<W: Write> {
    out: W,
}

impl<W: Write> CursorStyleGuard<W> {
    /// Take ownership of `out`; the returned guard resets the cursor on drop.
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> Drop for CursorStyleGuard<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, SetCursorStyle::DefaultUserShape);
    }
}

/// RAII guard for terminal bracketed paste: turns the mode on at creation and
/// **off on drop**, including on a panic unwind. With it on, the terminal hands
/// a paste to the client as a single [`Event::Paste`] carrying the whole text,
/// so the client forwards one `nx_input` and the server does one redraw —
/// instead of the per-character storm an unbracketed paste produces, which
/// makes pasted text trickle in one char at a time. Drops cleanly on the panic
/// path too, so a crash never leaves the terminal in bracketed-paste mode.
/// Generic over the writer for the same in-memory testability as
/// [`MouseCapture`]; production uses `std::io::stdout()`.
pub struct BracketedPaste<W: Write> {
    out: W,
}

impl<W: Write> BracketedPaste<W> {
    /// Enable bracketed paste on `out`; the returned guard disables it on drop.
    pub fn enable(mut out: W) -> Self {
        let _ = crossterm::execute!(out, EnableBracketedPaste);
        Self { out }
    }
}

impl<W: Write> Drop for BracketedPaste<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, DisableBracketedPaste);
    }
}

/// Builds a replacement backend for a `nx.session.reconnect` swap (§B). The swap loop
/// hands it the raw `nx_session_reconnect` params and gets back the client-side transport
/// of a fresh session (same stream type). The binary provides it — it owns session +
/// server-thread lifecycle — so the TUI stays agnostic to the transport and spec shape,
/// forwarding the params verbatim. Runs on the blocking pool (the handshake can take
/// seconds), so the current session keeps rendering while it builds.
pub type SessionBuilder<S> = std::sync::Arc<dyn Fn(Vec<Value>) -> Result<S> + Send + Sync>;

/// What [`event_loop`] reports back to the swap loop in [`run`].
enum Outcome<S> {
    /// The server asked the UI to exit, or the connection closed.
    Exit,
    /// A `nx.session.reconnect` build succeeded — re-attach onto this new transport,
    /// keeping the terminal (the "reload window").
    Swap(S),
}

/// Run the client, keeping the window across `nx.session.reconnect` swaps (§B).
///
/// The terminal (raw mode, alternate screen, mouse capture, the panic-restore hook) is set
/// up ONCE here and reused across swaps, so a reload onto a new backend never tears the
/// screen down. The inner [`event_loop`] runs on the current transport until the server
/// exits or a swap build succeeds; on a swap we re-attach onto the new transport (the old
/// one drops, winding its server down). `build` brings up the replacement session.
///
/// ratatui's `init`/`restore` own raw mode and the alternate screen (and a panic hook that
/// restores the terminal), so the user's shell is never left broken. Mouse capture is ours
/// to manage — a [`MouseCapture`] guard disables it on drop so even a panic in the event
/// loop can't leave mouse reporting on.
pub async fn run<S>(initial: S, build: SessionBuilder<S>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut terminal = ratatui::init();
    // Clear the screen once on entry (neovim emits `ESC[H ESC[J` here). ratatui
    // renders by diffing against its *own* previous buffer, which assumes the
    // alternate screen is blank when we arrive — true in most terminals, but not
    // guaranteed when nxvim runs *inside* another terminal emulator (e.g. our own
    // `:term`, or tmux) that doesn't blank the alt screen on entry. Without this,
    // cells the first frame leaves blank are never painted, so stale content shows
    // through as "render leftover". The explicit clear makes the baseline real.
    let _ = terminal.clear();
    // Capture mouse events so the panel's `[X]` is clickable.
    let mouse = MouseCapture::enable(std::io::stdout());
    // Restore the user's cursor shape on the way out — the loop switches it to a
    // bar in insert mode and must not leak that into their shell.
    let cursor = CursorStyleGuard::new(std::io::stdout());
    // Receive a paste as one event instead of one keystroke per character.
    let paste = BracketedPaste::enable(std::io::stdout());
    // Swap loop: re-attach onto each new transport a session-reconnect delivers, keeping
    // the terminal up. Exit / a fatal error ends it.
    let mut stream = initial;
    let result = loop {
        match event_loop(stream, &mut terminal, build.clone()).await {
            Ok(Outcome::Exit) => break Ok(()),
            Ok(Outcome::Swap(next)) => stream = next,
            Err(e) => break Err(e),
        }
    };
    // Restore terminal modes before leaving the alternate screen.
    drop(cursor); // reset cursor shape
    drop(paste); // disable bracketed paste
    drop(mouse); // disable mouse capture
    ratatui::restore();
    result
}

async fn event_loop<S>(
    stream: S,
    terminal: &mut DefaultTerminal,
    build: SessionBuilder<S>,
) -> Result<Outcome<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let size = terminal.size()?;
    let (reader, writer) = tokio::io::split(stream);
    let (rpc, mut incoming) = connect(reader, writer);

    // A `nx.session.reconnect` build (spawned on the blocking pool) delivers its result
    // here — `Ok(stream)` to swap onto, or `Err` to report and keep the current session.
    let (built_tx, mut built_rx) = tokio::sync::mpsc::unbounded_channel::<Result<S>>();

    // Fail loud if the attach itself fails (a broken transport / a server-side
    // error): swallowing it would leave the client running unattached — it would
    // then exit "successfully" without ever having painted a frame.
    rpc.request(
        "nx_ui_attach",
        vec![
            Value::from(size.width as u64),
            Value::from(text_height(size.height) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .map_err(|e| anyhow::anyhow!("nx_ui_attach failed: {e}"))?;

    // Remote (daemon-session) image previews: the file lives on the daemon, so the
    // store fetches its bytes over `nxvim_image_read` instead of reading local disk.
    // `img_fetch_*` carries a request out of the (synchronous) paint into the loop,
    // which issues the RPC on a spawned task; `img_bytes_*` carries the reply back.
    let (img_fetch_tx, mut img_fetch_rx) =
        tokio::sync::mpsc::unbounded_channel::<images::ImageFetch>();
    let (img_bytes_tx, mut img_bytes_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, (u64, u64), Result<Vec<u8>, String>)>();

    // The image renderer for `'imagepreview'`: detect the terminal's graphics
    // protocol now (it queries over stdio), *before* the `EventStream` below starts
    // reading input, so the two don't race for the terminal's replies.
    let mut image_store = images::ImageStore::new(img_fetch_tx);

    let mut view = View::default();
    let mut anim: Option<Animation> = None;
    let mut term_events = EventStream::new();
    // The cursor shape last sent to the terminal, so we re-emit the escape only
    // when the mode actually changes the shape rather than on every redraw.
    let mut cursor_shape: Option<SetCursorStyle> = None;
    // Armed by each keystroke; when the `TIMEOUT_LEN` branch wins (no further input
    // arrived), we send one `nxvim_input_flush` and disarm. The `sleep` is recreated
    // each loop pass, so any event — including the next key — restarts the countdown,
    // which is exactly `timeoutlen`'s reset-on-input semantics.
    let mut flush_armed = false;
    // Mouse drag-scroll: while the left button is held with the pointer parked in
    // the top/bottom edge band, re-send that drag on a timer so the buffer keeps
    // auto-scrolling without further pointer motion (the terminal only reports a
    // drag when the pointer actually moves). `Some(cell)` holds the cell to repeat;
    // cleared on release or when the pointer leaves the edge band.
    let mut autoscroll: Option<(u16, u16)> = None;

    loop {
        tokio::select! {
            term_event = term_events.next() => match term_event {
                Some(Ok(Event::Key(key))) => {
                    if key.kind != KeyEventKind::Release {
                        if let Some(notation) = encode_key(key) {
                            rpc.notify("nx_input", vec![Value::from(notation.as_str())]);
                            flush_armed = true;
                        }
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    // Bracketed paste: the whole clipboard arrives as one event,
                    // so forward it as a single `nx_input` (one redraw) rather
                    // than the per-character trickle of an unbracketed paste.
                    let notation = encode_paste(&text);
                    if !notation.is_empty() {
                        rpc.notify("nx_input", vec![Value::from(notation.as_str())]);
                        flush_armed = true;
                    }
                }
                Some(Ok(Event::Resize(w, h))) => {
                    rpc.notify(
                        "nx_ui_try_resize",
                        vec![Value::from(w as u64), Value::from(text_height(h) as u64)],
                    );
                    // A resize moves the chrome (the gutter width, the status row, the
                    // command line all shift), so cells that held chrome at the old size
                    // may be blank at the new one. ratatui resets its diff baseline on
                    // resize but emits no screen clear, so — on a host that doesn't clear
                    // its grid on resize — those old cells would linger. Clear so the next
                    // frame repaints over a known-blank screen (neovim clears here too).
                    let _ = terminal.clear();
                }
                Some(Ok(Event::Mouse(m))) => match m.kind {
                    // A left-press: forward the global cell to the server. The core
                    // owns the hit-test back to a window + buffer position (focus
                    // follows the click, the cursor lands there) or an overlay — the
                    // completion popup, a picker — under the pointer. `grid` is 0;
                    // nxvim is single-grid.
                    MouseEventKind::Down(MouseButton::Left) => {
                        let size = terminal.size().unwrap_or_default();
                        send_mouse(&rpc, "left", "press", &mouse_modifier(m.modifiers), m.row, m.column);
                        // Arm edge auto-scroll if the press already landed in the edge
                        // band (a press-and-hold there scrolls without a drag).
                        autoscroll = in_scroll_zone(m.row, size.height).then_some((m.row, m.column));
                    }
                    // Drag and release of the left button drive a text-area
                    // selection: the server extends Visual from the press anchor on
                    // drag, and keeps it on release. Forwarded unconditionally — the
                    // server no-ops them unless a text press set an anchor, so a
                    // stray drag over chrome does nothing.
                    MouseEventKind::Drag(MouseButton::Left) => {
                        send_mouse(&rpc, "left", "drag", &mouse_modifier(m.modifiers), m.row, m.column);
                        // (Re)arm continuous auto-scroll when the drag is parked in
                        // the edge band; disarm once it moves back into the body.
                        let size = terminal.size().unwrap_or_default();
                        autoscroll =
                            in_scroll_zone(m.row, size.height).then_some((m.row, m.column));
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        autoscroll = None; // release ends the drag, so stop scrolling
                        send_mouse(&rpc, "left", "release", &mouse_modifier(m.modifiers), m.row, m.column);
                    }
                    // Right / middle press: no client-owned overlay claims them, so
                    // forward straight to the server, which owns the gesture — the
                    // `'mousemodel'` right-click branch (extend / popup-setpos) and
                    // middle-click paste of the `"*` register. Only the press is
                    // meaningful (the server no-ops right/middle drag + release).
                    MouseEventKind::Down(button @ (MouseButton::Right | MouseButton::Middle)) => {
                        let name = if button == MouseButton::Right {
                            "right"
                        } else {
                            "middle"
                        };
                        send_mouse(&rpc, name, "press", &mouse_modifier(m.modifiers), m.row, m.column);
                    }
                    // The mouse wheel: forward every notch to the server, which owns
                    // the hit-test back to the window — or the overlay (the completion
                    // popup) — under the pointer (grid 0 — nxvim is single-grid).
                    MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => {
                        let action = match m.kind {
                            MouseEventKind::ScrollDown => "down",
                            MouseEventKind::ScrollUp => "up",
                            MouseEventKind::ScrollRight => "right",
                            _ => "left",
                        };
                        send_mouse(&rpc, "wheel", action, &mouse_modifier(m.modifiers), m.row, m.column);
                    }
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Ok(Outcome::Exit),
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        view.update(&params);
                        anim = arm_animation(&view, anim.take());
                        draw_frame(terminal, &view, anim.as_ref(), &mut image_store)?;
                        // Match the cursor shape to the mode (a thin bar in insert
                        // mode). Emitted only on change so it doesn't flicker.
                        let want = cursor_style(&view);
                        if cursor_shape != Some(want) {
                            let _ = crossterm::execute!(std::io::stdout(), want);
                            cursor_shape = Some(want);
                        }
                    }
                    "nxvim_exit" => return Ok(Outcome::Exit),
                    // `nx.session.reconnect(spec)` from inside the VM (§B): bring up the new
                    // backend OFF the event loop (the handshake can take seconds) so this
                    // session keeps rendering meanwhile; the result arrives on `built_rx`.
                    // The spec params are forwarded verbatim to the binary's builder.
                    "nx_session_reconnect" => {
                        let build = build.clone();
                        let tx = built_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = tx.send(build(params));
                        });
                    }
                    // `:connect <url>` with no matching connect-provider (§C): the raw URL
                    // rides as the single param. Forwarded verbatim to the SAME builder — it
                    // distinguishes a fallback URL (a string) from a `nx.session.reconnect`
                    // spec (a map) and dials it directly (nxvim:// / ssh host). Built off the
                    // event loop so this session keeps rendering while the handshake runs.
                    "nx_connect_fallback" => {
                        let build = build.clone();
                        let tx = built_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = tx.send(build(params));
                        });
                    }
                    _ => {}
                },
                Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                None => return Ok(Outcome::Exit),
            },
            // A session-reconnect build finished. On success, swap onto the new transport
            // (this session's `rpc`/`incoming` drop as we return, winding its server down);
            // on failure, keep the current session and report why — no half-swap.
            built = built_rx.recv() => match built {
                Some(Ok(next)) => return Ok(Outcome::Swap(next)),
                Some(Err(e)) => {
                    let line = format!("session reconnect failed: {e:#}")
                        .replace('\n', "; ")
                        .replace('\'', "''");
                    rpc.notify("nx_command", vec![Value::from(format!("echoerr '{line}'"))]);
                }
                None => {} // `built_tx` is held above; unreachable
            },
            // Animation frame tick (~60fps). Disabled when nothing is animating,
            // so the future is never even created in the idle case.
            _ = sleep(Duration::from_millis(16)), if anim.is_some() => {
                if anim.as_ref().is_some_and(|a| a.start.elapsed() >= a.duration) {
                    anim = None; // settle: render the destination view below
                }
                draw_frame(terminal, &view, anim.as_ref(), &mut image_store)?;
            },
            // `timeoutlen` idle flush: a keystroke armed the timer and nothing
            // followed within `'timeoutlen'`, so nudge the server to resolve any key
            // it withheld as a live prefix (design D4). Harmless when nothing is
            // pending. Disarmed so it fires at most once per idle gap. The duration
            // and the whole arm come from the relayed `'timeout'`/`'timeoutlen'`:
            // under `notimeout` the branch is disabled, so a withheld mapped prefix
            // waits forever (a which-key popup stays up) instead of being flushed.
            _ = sleep(Duration::from_millis(view.timeoutlen)), if flush_armed && view.timeout => {
                rpc.notify("nxvim_input_flush", vec![]);
                flush_armed = false;
            },
            // Continuous mouse drag-scroll: the button is held with the pointer in
            // the edge band, so re-issue the drag at its last cell. The server
            // scrolls the focused window one line per drag it lands outside the
            // text body and re-extends the selection; held still, this paces it.
            _ = sleep(AUTOSCROLL_INTERVAL), if autoscroll.is_some() => {
                if let Some((row, col)) = autoscroll {
                    send_mouse(&rpc, "left", "drag", "", row, col);
                }
            },
            // A remote preview needs its bytes: fetch them over `nxvim_image_read` on a
            // spawned task (so a slow daemon read never stalls input/redraws) and send
            // the reply back on `img_bytes_*`. `None` (the store dropped) just falls
            // through. The closure-side paint can only enqueue, not await, hence this.
            fetch = img_fetch_rx.recv() => if let Some(images::ImageFetch { path, version }) = fetch {
                let rpc = rpc.clone();
                let tx = img_bytes_tx.clone();
                tokio::spawn(async move {
                    let result = match rpc
                        .request("nxvim_image_read", vec![Value::from(path.as_str())])
                        .await
                    {
                        Ok(Value::Binary(bytes)) => Ok(bytes),
                        Ok(other) => Err(format!("nxvim_image_read: unexpected reply {other:?}")),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send((path, version, result));
                });
            },
            // A remote preview's bytes arrived (or the read failed): hand them to the
            // store and repaint, so the picture replaces its loading placeholder.
            bytes = img_bytes_rx.recv() => if let Some((path, version, result)) = bytes {
                image_store.deliver(path, version, result);
                draw_frame(terminal, &view, anim.as_ref(), &mut image_store)?;
            },
        }
    }
}

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}

/// Forward one mouse gesture to the server as an `nx_input_mouse` notification.
/// `button`/`action` name the gesture (e.g. `"left"`/`"press"`, `"wheel"`/`"down"`),
/// `mods` is the [`mouse_modifier`] string, and `row`/`col` the global cell. `grid`
/// is always `0` — nxvim is single-grid.
fn send_mouse(rpc: &Rpc, button: &str, action: &str, mods: &str, row: u16, col: u16) {
    rpc.notify(
        "nx_input_mouse",
        vec![
            Value::from(button),
            Value::from(action),
            Value::from(mods),
            Value::from(0u64),
            Value::from(row as u64),
            Value::from(col as u64),
        ],
    );
}

/// Paint the current `view` (mid-`anim` when one is in flight) into `terminal` via
/// the shared [`render`]. The three event-loop repaint sites — a `redraw`, an
/// animation tick, and a delivered image — all funnel through here.
fn draw_frame(
    terminal: &mut DefaultTerminal,
    view: &View,
    anim: Option<&Animation>,
    image_store: &mut images::ImageStore,
) -> Result<()> {
    terminal.draw(|frame| render(frame, view, anim, 0, Some(image_store)))?;
    Ok(())
}

/// The `nx_input_mouse` modifier string for a crossterm mouse event's
/// modifiers — e.g. Shift+Ctrl → `"CS"`. The server's parser accepts the chars in
/// any order with the `-` separator optional, so concatenation is enough. Drives
/// shift-click (extend the selection) and the future Ctrl/Alt gestures.
fn mouse_modifier(mods: KeyModifiers) -> String {
    let mut s = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        s.push('C');
    }
    if mods.contains(KeyModifiers::SHIFT) {
        s.push('S');
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push('A');
    }
    s
}
