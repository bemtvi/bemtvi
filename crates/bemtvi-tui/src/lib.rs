//! The terminal UI client.
//!
//! A thin RPC client that owns no editor state. It attaches to the server,
//! sends keystrokes as vim key-notation (`btv_input`), and renders the
//! server's [`View`](bemtvi_view::View) using **ratatui-native widgets, one per
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
//! [`bemtvi_view`] crate (it mirrors the server's view, parses each `redraw`, and
//! holds the msgpack accessors) — including the scroll-slide state machine
//! ([`ScrollAnim`](bemtvi_view::ScrollAnim) / [`arm_scroll`](bemtvi_view::arm_scroll))
//! this client drives from its clock. The TUI-specific work is split across
//! submodules: [`render`] paints the frame, [`images`] renders `'imagepreview'`
//! pictures via ratatui-image, and [`keys`] encodes key events. This module keeps
//! only the event loop and transport.

mod images;
mod keys;
mod render;
mod signals;
mod termquery;

pub use keys::encode_key;
pub use render::{cursor_style, paint, paint_with_cursor, ScrollHarness};
pub use signals::{exit_as_signal_if_killed, install as install_signal_restore, ShutdownSignal};
pub use termquery::{has_status_report, parse_term_caps, term_names_a_multiplexer, TermCaps};

use anyhow::Result;
use bemtvi_rpc::{connect, Incoming, Rpc};
use bemtvi_view::{encode_paste, View};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures::StreamExt;
use rmpv::Value;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

use bemtvi_view::{arm_scroll, ScrollAnim};
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
/// so the client forwards one `btv_input` and the server does one redraw —
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

/// RAII guard for the **kitty keyboard protocol** (progressive enhancement): pushes
/// the `DISAMBIGUATE_ESCAPE_CODES` flag on creation and **pops it on drop**,
/// including on a panic unwind. With it on, a supporting terminal reports modified
/// keys the legacy encoding cannot express — `<S-CR>`, `<C-CR>`, `<C-S-…>`, a lone
/// `<Esc>` unambiguously — as CSI-u sequences crossterm decodes back into the right
/// `KeyEvent`. Without it those chords are byte-identical to their unmodified form
/// (Shift+Enter == Enter), so a `<S-CR>` mapping can never fire.
///
/// `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES` is requested — byte-identical to
/// what neovim pushes (`CSI > 3 u`). `DISAMBIGUATE` is what makes Ctrl+I/Ctrl+M/… CSI-u
/// (distinct from Tab/Enter/…); some terminals only actually switch on the enhanced
/// encoding once `REPORT_EVENT_TYPES` rides along, so we match neovim's pair rather
/// than pushing `DISAMBIGUATE` alone (which left Ctrl+I arriving as a bare `<Tab>` on
/// real terminals). `REPORT_ALL_KEYS_AS_ESCAPE_CODES` is deliberately NOT set — that
/// re-encodes plain text too. The extra key-*release* events `REPORT_EVENT_TYPES`
/// adds are dropped in the input loop (`key.kind != Release`); a *repeat* is treated
/// as a press, exactly like the legacy repeated-press autorepeat.
///
/// The caller ([`run`]) constructs this only when [`kitty_keyboard_enabled`] says the
/// terminal supports the protocol (detected, or forced via `BEMTVI_KITTY_KEYBOARD`), so
/// the same decision drives both the push here and the `keyboard_protocol` capability
/// reported to the server — the two must agree or a distinct `<C-i>` the terminal
/// can't send would be parsed anyway. Like [`MouseCapture`] the guard is generic over
/// the writer for in-memory testing; production uses `std::io::stdout()`.
pub struct KeyboardEnhancement<W: Write> {
    out: W,
}

impl<W: Write> KeyboardEnhancement<W> {
    /// Push the `DISAMBIGUATE_ESCAPE_CODES` enhancement flag on `out`; the returned
    /// guard pops it on drop.
    pub fn push(mut out: W) -> Self {
        let _ = crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        );
        Self { out }
    }
}

impl<W: Write> Drop for KeyboardEnhancement<W> {
    fn drop(&mut self) {
        let _ = crossterm::execute!(self.out, PopKeyboardEnhancementFlags);
    }
}

/// Whether to enable the kitty keyboard protocol at startup. By default this reads
/// the answer out of the one capability round [`termquery::probe`] ran — the
/// terminal replied to the progressive-enhancement flags query, so it speaks the
/// protocol.
///
/// Detection deliberately does *not* go through
/// [`crossterm::terminal::supports_keyboard_enhancement`]: that asks its own
/// question on its own timeout, through crossterm's internal reader, which both
/// adds a serial probe in front of the first frame and competes with our `poll(2)`
/// for the same bytes. On non-unix, where [`termquery::probe`] asks nothing, it is
/// still the only probe available.
///
/// `BEMTVI_KITTY_KEYBOARD` overrides detection in either direction: a falsey value
/// (`0`/`false`/`off`/`no`, case-insensitive, or empty) forces it **off** for a
/// terminal that mishandles the flags; any other value forces it **on**, the escape
/// hatch for a supporting terminal whose probe reply raced and read as unsupported.
fn kitty_keyboard_enabled(caps: &TermCaps) -> bool {
    #[cfg(not(unix))]
    let _ = caps;
    match std::env::var("BEMTVI_KITTY_KEYBOARD").ok().as_deref() {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        #[cfg(unix)]
        None => caps.kitty_keyboard,
        #[cfg(not(unix))]
        None => crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false),
    }
}

/// Whether the terminal supports truecolor (24-bit RGB), the "rich colors" the
/// server's bundled `bemtvi` colorscheme needs. Detected from `COLORTERM`, the de
/// facto capability signal every modern truecolor terminal exports (`truecolor`
/// or `24bit`). Reported to the server as the `truecolor` attach capability, which
/// auto-loads `:colorscheme bemtvi` when the user's config hasn't already picked one
/// — so a rich terminal lands on real colors with zero config, while a legacy /
/// 256-color terminal keeps its own palette (a downgraded truecolor scheme reads
/// worse than the terminal's tuned ANSI set).
///
/// `BEMTVI_TRUECOLOR` overrides detection in either direction, matching
/// [`kitty_keyboard_enabled`]: a falsey value (`0`/`false`/`off`/`no`, or empty)
/// forces it **off**; any other value forces it **on** — the escape hatch for a
/// truecolor terminal that doesn't set `COLORTERM` (e.g. some `TERM=*-direct`
/// setups), or to suppress the default scheme on a terminal that does.
fn truecolor_enabled() -> bool {
    match std::env::var("BEMTVI_TRUECOLOR").ok().as_deref() {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        None => matches!(
            std::env::var("COLORTERM").ok().as_deref(),
            Some("truecolor") | Some("24bit")
        ),
    }
}

/// Whether the terminal can be handed a clipboard write — an **OSC 52** escape,
/// which asks the terminal emulator to put text on *its own* machine's clipboard.
///
/// This is what makes `"+y` work over ssh. Nothing running on the remote host can
/// reach the clipboard the user actually pastes from; the terminal in front of
/// them can, and the escape rides back down the same pipe the screen does. The
/// server falls back to it (as the `osc52` attach capability) only when it found
/// no usable clipboard tool of its own.
///
/// Gating on real support is load-bearing, not politeness — same reasoning as
/// [`kitty_keyboard_enabled`]. Reporting it unconditionally would make every `"+y`
/// on a terminal that ignores the escape *look* like it copied while the text went
/// nowhere; a copy that silently vanishes is worse than a loud "no clipboard
/// provider". So we ask, the way neovim does: DA1 (`ESC[c`), whose reply lists
/// capability `52` on terminals that implement the clipboard extension, with
/// XTGETTCAP for the `Ms` capability as a fallback for terminals that answer that
/// instead. Both questions ride the one capability round [`termquery::probe`] runs,
/// so neither costs a wait of its own.
///
/// **Behind a multiplexer, an unanswered question is not a "no".** tmux and screen
/// answer our queries *themselves*: tmux's device attributes never list `52` (they
/// describe tmux, not the emulator on the far end) and it ignores XTGETTCAP
/// entirely, so the probe learns nothing about the terminal the user is actually
/// looking at — and there is no way to ask it, since a passthrough-wrapped query
/// needs `allow-passthrough`, which is off by default and can't be turned on from
/// the far side of an ssh hop. Taking that silence as "no" is what made `"+y`
/// unavailable in every tmux session, which is the *worst* answer available:
/// the multiplexer is exactly the case OSC 52 exists for (a remote host with no
/// clipboard of its own), and both tmux (`set-clipboard`, default `external`) and
/// screen forward the escape outward by default. So a multiplexer counts as
/// support: a very likely yank beats a certain failure. A user who turned
/// forwarding off wants `BEMTVI_OSC52=0`.
///
/// `BEMTVI_OSC52` overrides detection in either direction, matching
/// [`truecolor_enabled`]: a falsey value (`0`/`false`/`off`/`no`, or empty) forces
/// it **off** — for a `set-clipboard off` multiplexer, or a terminal that ignores
/// the escape; any other value forces it **on**, for a terminal that supports the
/// write but advertises nothing, and the only way to enable it where there is no
/// tty to probe.
fn osc52_enabled(caps: &TermCaps) -> bool {
    match std::env::var("BEMTVI_OSC52").ok().as_deref() {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        None => caps.osc52 || caps.multiplexer,
    }
}

/// Whether a Primary Device Attributes reply lists capability **52** — "can set
/// the clipboard via OSC 52".
///
/// The reply is `ESC [ ? <param> ; <param> ; … c`; a terminal may send other
/// output around it, so this scans for the introducer rather than anchoring at the
/// start. Public for the client-side test (`tests/osc52.rs`) — parsing a reply
/// needs no terminal.
pub fn da1_advertises_osc52(reply: &[u8]) -> bool {
    da1_lists(reply, b"52")
}

/// Whether a Primary Device Attributes reply lists capability **4** — sixel
/// graphics. Read from the same reply as [`da1_advertises_osc52`] (one DA1 answers
/// both questions), and public for the client-side test for the same reason.
pub fn da1_advertises_sixel(reply: &[u8]) -> bool {
    da1_lists(reply, b"4")
}

/// Whether the device attributes report in `reply` lists `param`.
///
/// Matching is per-parameter, never substring: a stray `52` inside `152`/`520`
/// would otherwise read as clipboard support, and a truncated report (no final
/// `c`) would read as a complete one. [`termquery::csi_question_params`] enforces
/// both by only returning the parameters of a *terminated* `CSI ? … c`.
fn da1_lists(reply: &[u8], param: &[u8]) -> bool {
    termquery::csi_question_params(reply, b'c').is_some_and(|params| params.contains(&param))
}

/// Whether an XTGETTCAP reply says the `Ms` capability *is* an OSC 52 sequence.
///
/// A successful reply is `ESC P 1 + r <name-hex> = <value-hex> ESC \`; a terminal
/// that doesn't know the capability answers `ESC P 0 + r … ESC \`. The value is
/// only useful if it really is OSC 52 — a terminal reporting some other clipboard
/// mechanism must not be sent one (bemtvi, like neovim, emits OSC 52 and nothing
/// else). Public for the client-side test, as with [`da1_advertises_osc52`].
pub fn xtgettcap_advertises_osc52(reply: &[u8]) -> bool {
    let Some(at) = termquery::find(reply, b"\x1bP1+r") else {
        return false;
    };
    let body = &reply[at + 5..];
    let end = body.iter().position(|&b| b == 0x1b).unwrap_or(body.len());
    let Some(eq) = body[..end].iter().position(|&b| b == b'=') else {
        return false;
    };
    let Some(value) = unhex(&body[eq + 1..end]) else {
        return false;
    };
    value.starts_with(b"\x1b]52")
}

/// Whether `seq` is a well-formed OSC 52 clipboard write —
/// `ESC ] 52 ; c ; <base64> ESC \` — the only raw terminal sequence the server is
/// allowed to hand us to emit verbatim (a `"+` yank, see `clipboard.rs::osc52_sequence`).
///
/// This is a **fail-closed whitelist**, not a pass-through: the bytes come from a
/// server we don't control, and an arbitrary escape sequence written to the
/// terminal could reprogram keys, dump the screen, or exfiltrate clipboard state.
/// A payload whose bytes are all base64 alphabet characters can't contain ESC or
/// any other control byte, so a passing sequence cannot terminate early or smuggle
/// a second escape in — and anything that isn't exactly this shape is dropped.
fn is_osc52(seq: &str) -> bool {
    let Some(payload) = seq
        .strip_prefix("\x1b]52;")
        .and_then(|rest| rest.strip_suffix("\x1b\\"))
    else {
        return false;
    };
    let mut parts = payload.splitn(2, ';');
    match (parts.next(), parts.next()) {
        // Selection `c` — the only selection bemtvi writes; `"*`/`"+` share one provider.
        (Some("c"), Some(b64)) => {
            !b64.is_empty()
                && b64
                    .bytes()
                    .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'='))
        }
        _ => false,
    }
}

/// Decode an even-length ASCII-hex string, or `None` if it isn't one.
fn unhex(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    hex.chunks(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(s, 16).ok()
        })
        .collect()
}

/// Builds a replacement backend for a `btv.session.reconnect` swap (§B). The swap loop
/// hands it the raw `btv_session_reconnect` params and gets back the client-side transport
/// of a fresh session (same stream type). The binary provides it — it owns session +
/// server-thread lifecycle — so the TUI stays agnostic to the transport and spec shape,
/// forwarding the params verbatim. Runs on the blocking pool (the handshake can take
/// seconds), so the current session keeps rendering while it builds.
pub type SessionBuilder<S> = std::sync::Arc<dyn Fn(Vec<Value>) -> Result<S> + Send + Sync>;

/// What this client tells the server about the terminal behind it, plus the raw
/// probe answers the parts of the client that render need.
///
/// Detected once in [`run`] and carried across every `btv.session.reconnect` swap:
/// the terminal doesn't change when the backend does, and re-probing mid-session
/// would read the user's keystrokes instead of a reply.
#[derive(Clone, Copy, Debug)]
struct AttachCaps {
    /// The kitty keyboard protocol is *on* — the flags were pushed, so the server
    /// can parse a distinct `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>`.
    keyboard_protocol: bool,
    /// 24-bit color (see [`truecolor_enabled`]).
    truecolor: bool,
    /// OSC 52 clipboard writes (see [`osc52_enabled`]).
    osc52: bool,
    /// The terminal's own answers, for the renderer (image protocol, cell size).
    term: TermCaps,
}

/// What [`event_loop`] reports back to the swap loop in [`run`].
enum Outcome<S> {
    /// The server asked the UI to exit, or the connection closed.
    Exit,
    /// A `btv.session.reconnect` build succeeded — re-attach onto this new transport,
    /// keeping the terminal (the "reload window").
    Swap(S),
}

/// Run the client, keeping the window across `btv.session.reconnect` swaps (§B).
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
/// loop can't leave mouse reporting on. All of that covers *unwinding* exits only, so
/// [`signals::install`] covers the rest: a `kill` (SIGTERM) or a closed window (SIGHUP)
/// runs no destructor at all, and instead winds the session down through the event loop's
/// `shutdown` arm — or, if that can't be reached, restores the terminal from the signal
/// handler itself.
pub async fn run<S>(initial: S, build: SessionBuilder<S>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Before raw mode goes on, so the termios the handler restores is the user's own.
    let mut shutdown = signals::install();
    let mut terminal = ratatui::init();
    // Clear the screen once on entry (neovim emits `ESC[H ESC[J` here). ratatui
    // renders by diffing against its *own* previous buffer, which assumes the
    // alternate screen is blank when we arrive — true in most terminals, but not
    // guaranteed when bemtvi runs *inside* another terminal emulator (e.g. our own
    // `:term`, or tmux) that doesn't blank the alt screen on entry. Without this,
    // cells the first frame leaves blank are never painted, so stale content shows
    // through as "render leftover". The explicit clear makes the baseline real.
    let _ = terminal.clear();
    // Ask the terminal everything we need to know about it — ONCE, in a single round
    // trip (see [`termquery`]). Every capability below is read out of this one answer:
    // asking them one at a time put each unanswered question's timeout in front of the
    // first frame, which is what made a multiplexer or a two-hop ssh feel like a hang.
    // It must run here — raw mode is on (`ratatui::init`) but the `EventStream` that
    // would race for the replies isn't up until `event_loop`.
    let caps = termquery::probe();
    // Enable the kitty keyboard protocol WHEN THE TERMINAL SUPPORTS IT, so modified
    // keys the legacy encoding can't express (`<S-CR>`, `<C-CR>`, `<C-S-…>`, an
    // unambiguous lone `<Esc>`) reach the server as distinct keys. Gating on real
    // support is load-bearing, not just politeness: the client reports the same
    // decision to the server as its `keyboard_protocol` capability, and the server
    // then parses a *distinct* `<C-i>` / `<C-m>` / … only when the terminal can
    // actually deliver one. Push-and-assume on a terminal that ignores the push (e.g.
    // WezTerm with `enable_kitty_keyboard` off) would desync the two: the terminal
    // still sends `<Tab>` for Ctrl+I while the server waits for a `<C-i>` that never
    // comes, so the map dies. Detection must run here — raw mode is on (`ratatui::init`)
    // but the `EventStream` that would race for the query reply isn't up until
    // `event_loop`. `None` ⇒ legacy encoding, and nothing to pop on exit.
    let keyboard =
        kitty_keyboard_enabled(&caps).then(|| KeyboardEnhancement::push(std::io::stdout()));
    // Whether this terminal can show 24-bit color, reported to the server so it can
    // default in the bundled `bemtvi` colorscheme (see `truecolor_enabled`). A pure
    // capability report — no terminal state to set up or tear down — so unlike the
    // guards above it's just a bool carried into each `event_loop`/attach.
    let truecolor = truecolor_enabled();
    // Whether this terminal accepts an OSC 52 clipboard write, reported to the
    // server so it can back `"+` / `"*` with the terminal when the host it runs on
    // has no clipboard of its own — the ssh case (see `osc52_enabled`).
    let osc52 = osc52_enabled(&caps);
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
        match event_loop(
            stream,
            &mut terminal,
            build.clone(),
            AttachCaps {
                keyboard_protocol: keyboard.is_some(),
                truecolor,
                osc52,
                term: caps,
            },
            &mut shutdown,
        )
        .await
        {
            Ok(Outcome::Exit) => break Ok(()),
            Ok(Outcome::Swap(next)) => stream = next,
            Err(e) => break Err(e),
        }
    };
    // Restore terminal modes before leaving the alternate screen.
    drop(cursor); // reset cursor shape
    drop(paste); // disable bracketed paste
    drop(mouse); // disable mouse capture
    drop(keyboard); // pop the kitty keyboard protocol flags (no-op if never pushed)
    ratatui::restore();
    result
}

async fn event_loop<S>(
    stream: S,
    terminal: &mut DefaultTerminal,
    build: SessionBuilder<S>,
    caps: AttachCaps,
    shutdown: &mut ShutdownSignal,
) -> Result<Outcome<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let size = terminal.size()?;
    let (reader, writer) = tokio::io::split(stream);
    let (rpc, mut incoming) = connect(reader, writer);

    // A `btv.session.reconnect` build (spawned on the blocking pool) delivers its result
    // here — `Ok(stream)` to swap onto, or `Err` to report and keep the current session.
    let (built_tx, mut built_rx) = tokio::sync::mpsc::unbounded_channel::<Result<S>>();

    // Fail loud if the attach itself fails (a broken transport / a server-side
    // error): swallowing it would leave the client running unattached — it would
    // then exit "successfully" without ever having painted a frame.
    rpc.request(
        "btv_ui_attach",
        vec![
            Value::from(size.width as u64),
            Value::from(text_height(size.height) as u64),
            // Capabilities map: tell the server the kitty keyboard protocol is on so
            // it parses distinct `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>` instead of folding
            // them onto their named twins.
            Value::Map(vec![
                (
                    Value::from("keyboard_protocol"),
                    Value::from(caps.keyboard_protocol),
                ),
                // 24-bit color support: lets the server default in the bundled
                // `bemtvi` colorscheme on a rich terminal (see `truecolor_enabled`).
                (Value::from("truecolor"), Value::from(caps.truecolor)),
                // OSC 52 clipboard writes: lets the server back `"+` / `"*` with
                // this terminal when its own host has no clipboard tool that could
                // reach the user — the ssh case (see `osc52_enabled`).
                (Value::from("osc52"), Value::from(caps.osc52)),
            ]),
        ],
    )
    .await
    .map_err(|e| anyhow::anyhow!("btv_ui_attach failed: {e}"))?;

    // Remote (daemon-session) image previews: the file lives on the daemon, so the
    // store fetches its bytes over `bemtvi_image_read` instead of reading local disk.
    // `img_fetch_*` carries a request out of the (synchronous) paint into the loop,
    // which issues the RPC on a spawned task; `img_bytes_*` carries the reply back.
    let (img_fetch_tx, mut img_fetch_rx) =
        tokio::sync::mpsc::unbounded_channel::<images::ImageFetch>();
    let (img_bytes_tx, mut img_bytes_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, (u64, u64), Result<Vec<u8>, String>)>();

    // The image renderer for `'imagepreview'`: the terminal's graphics protocol and
    // cell size come out of the capability round `run` already ran, so nothing here
    // touches stdio — no second query to race the `EventStream` below.
    let mut image_store = images::ImageStore::new(img_fetch_tx, caps.term);

    let mut view = View::default();
    let mut anim: Option<ScrollAnim> = None;
    let mut term_events = EventStream::new();
    // The cursor shape last sent to the terminal, so we re-emit the escape only
    // when the mode actually changes the shape rather than on every redraw.
    let mut cursor_shape: Option<SetCursorStyle> = None;
    // Armed by each keystroke; when the `TIMEOUT_LEN` branch wins (no further input
    // arrived), we send one `bemtvi_input_flush` and disarm. The `sleep` is recreated
    // each loop pass, so any event — including the next key — restarts the countdown,
    // which is exactly `timeoutlen`'s reset-on-input semantics.
    let mut flush_armed = false;
    // Mouse drag-scroll: while the left button is held with the pointer parked in
    // the top/bottom edge band, re-send that drag on a timer so the buffer keeps
    // auto-scrolling without further pointer motion (the terminal only reports a
    // drag when the pointer actually moves). `Some(cell)` holds the cell to repeat;
    // cleared on release or when the pointer leaves the edge band.
    let mut autoscroll: Option<(u16, u16)> = None;
    // Set once we've asked the editor to quit because we were killed, so the request
    // goes out exactly once (and the `shutdown` arm stops competing for the loop).
    // Checked eagerly too: the signal may have landed while we were attaching, or
    // during a session swap, in which case the wake-up was consumed by the loop we
    // just left and only this flag remembers it.
    let mut winding_down = signals::shutdown_requested();
    if winding_down {
        request_graceful_quit(&rpc);
    }

    loop {
        tokio::select! {
            // A fatal signal (`kill`, a closed terminal window) asked us to stop.
            // Quit the way the user would, so the exit sequence runs and this
            // session's state is written — see `signals`.
            () = shutdown.recv(), if !winding_down => {
                winding_down = true;
                request_graceful_quit(&rpc);
            },
            term_event = term_events.next() => match term_event {
                Some(Ok(Event::Key(key))) => {
                    if key.kind != KeyEventKind::Release {
                        if let Some(notation) = encode_key(key) {
                            rpc.notify("btv_input", vec![Value::from(notation.as_str())]);
                            flush_armed = true;
                        }
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    // Bracketed paste: the whole clipboard arrives as one event,
                    // so forward it as a single `btv_input` (one redraw) rather
                    // than the per-character trickle of an unbracketed paste.
                    let notation = encode_paste(&text);
                    if !notation.is_empty() {
                        rpc.notify("btv_input", vec![Value::from(notation.as_str())]);
                        flush_armed = true;
                    }
                }
                Some(Ok(Event::Resize(w, h))) => {
                    rpc.notify(
                        "btv_ui_try_resize",
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
                    // bemtvi is single-grid.
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
                    // popup) — under the pointer (grid 0 — bemtvi is single-grid).
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
                        anim = arm_scroll(&view, anim.take());
                        draw_frame(terminal, &view, anim.as_ref(), &mut image_store)?;
                        // Match the cursor shape to the mode (a thin bar in insert
                        // mode). Emitted only on change so it doesn't flicker.
                        let want = cursor_style(&view);
                        if cursor_shape != Some(want) {
                            let _ = crossterm::execute!(std::io::stdout(), want);
                            cursor_shape = Some(want);
                        }
                    }
                    // Raw bytes for the *terminal* rather than the renderer: today an
                    // OSC 52 clipboard write behind a `"+` yank. The client writes
                    // **only** the OSC 52 sequence family and drops anything else —
                    // fail closed against a server (or compromised wire) shipping an
                    // arbitrary escape sequence straight at the user's terminal.
                    "btv_ui_send" => {
                        if let Some(seq) = params.first().and_then(|v| v.as_str()) {
                            use std::io::Write as _;
                            if is_osc52(seq) {
                                let mut out = std::io::stdout();
                                let _ = out.write_all(seq.as_bytes());
                                let _ = out.flush();
                            }
                        }
                    }
                    "bemtvi_exit" => return Ok(Outcome::Exit),
                    // `btv.session.reconnect(spec)` from inside the VM (§B): bring up the new
                    // backend OFF the event loop (the handshake can take seconds) so this
                    // session keeps rendering meanwhile; the result arrives on `built_rx`.
                    // The spec params are forwarded verbatim to the binary's builder.
                    "btv_session_reconnect" => {
                        let build = build.clone();
                        let tx = built_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = tx.send(build(params));
                        });
                    }
                    // `:connect <url>` with no matching connect-provider (§C): the raw URL
                    // rides as the single param. Forwarded verbatim to the SAME builder — it
                    // distinguishes a fallback URL (a string) from a `btv.session.reconnect`
                    // spec (a map) and dials it directly (bemtvi:// / ssh host). Built off the
                    // event loop so this session keeps rendering while the handshake runs.
                    "btv_connect_fallback" => {
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
                    rpc.notify("btv_command", vec![Value::from(format!("echoerr '{line}'"))]);
                }
                None => {} // `built_tx` is held above; unreachable
            },
            // Animation frame tick (~60fps). Disabled when nothing is animating,
            // so the future is never even created in the idle case.
            _ = sleep(Duration::from_millis(16)), if anim.is_some() => {
                if anim.as_ref().is_some_and(ScrollAnim::done) {
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
                rpc.notify("bemtvi_input_flush", vec![]);
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
            // A remote preview needs its bytes: fetch them over `bemtvi_image_read` on a
            // spawned task (so a slow daemon read never stalls input/redraws) and send
            // the reply back on `img_bytes_*`. `None` (the store dropped) just falls
            // through. The closure-side paint can only enqueue, not await, hence this.
            fetch = img_fetch_rx.recv() => if let Some(images::ImageFetch { path, version }) = fetch {
                let rpc = rpc.clone();
                let tx = img_bytes_tx.clone();
                tokio::spawn(async move {
                    let result = bemtvi_view::images::image_read_reply(
                        rpc.request("bemtvi_image_read", vec![Value::from(path.as_str())])
                            .await,
                    );
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

/// Ask the server to quit as `:qall!` does, the client's half of a graceful shutdown.
///
/// The bang is deliberate and so is the missing `w`: this must not stop at an E37
/// ("no write since last change") prompt nobody is there to answer, and it must not
/// write files the user never asked to write. What it *does* buy over dying on the
/// spot is the real exit sequence — `QuitPre`/`ExitPre`/`VimLeavePre`/`VimLeave`
/// autocmds, so plugins can persist their own state, and the server's clean-exit
/// shada flush (marks, registers, histories, the exit cursor). The server answers
/// with `bemtvi_exit` when it has finished, which is what actually ends the loop.
fn request_graceful_quit(rpc: &Rpc) {
    rpc.notify("btv_command", vec![Value::from("qall!")]);
}

/// Text-area height = terminal height minus the chrome rows we render ourselves.
fn text_height(terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(CHROME_ROWS).max(1)
}

/// Forward one mouse gesture to the server as an `btv_input_mouse` notification.
/// `button`/`action` name the gesture (e.g. `"left"`/`"press"`, `"wheel"`/`"down"`),
/// `mods` is the [`mouse_modifier`] string, and `row`/`col` the global cell. `grid`
/// is always `0` — bemtvi is single-grid.
fn send_mouse(rpc: &Rpc, button: &str, action: &str, mods: &str, row: u16, col: u16) {
    rpc.notify(
        "btv_input_mouse",
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
    anim: Option<&ScrollAnim>,
    image_store: &mut images::ImageStore,
) -> Result<()> {
    terminal.draw(|frame| render(frame, view, anim, Some(image_store)))?;
    Ok(())
}

/// The `btv_input_mouse` modifier string for a crossterm mouse event's modifiers —
/// the shared [`bemtvi_view::mouse_modifier`] over crossterm's flags.
fn mouse_modifier(mods: KeyModifiers) -> String {
    bemtvi_view::mouse_modifier(
        mods.contains(KeyModifiers::CONTROL),
        mods.contains(KeyModifiers::SHIFT),
        mods.contains(KeyModifiers::ALT),
    )
}
