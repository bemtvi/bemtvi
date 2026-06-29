//! Terminal-job buffers — the pure-core half of the `:terminal` feature.
//!
//! Core is pure and synchronous, so it owns no PTY and parses no escape
//! sequences. What lives here is exactly the part that *is* pure: the
//! [`TerminalOp`] queue core hands the server (open / input / kill), the
//! [`Editor::open_terminal`] entry point, the inbound
//! [`Editor::terminal_update`] / [`Editor::terminal_closed`] the server calls
//! when the child's screen changes, the [`Mode::Terminal`] keystroke→bytes
//! forwarding, and the keystroke→bytes translation itself ([`key_to_terminal_bytes`]).
//!
//! The byte transport (a real PTY, native) and the vt100 emulation (bytes →
//! screen grid → these `lines`) live server-side; see
//! `crates/nxvim-server/src/terminal.rs` and
//! `docs/plans/2026-06-14-terminal-in-buffer.md`.

use super::*;
use crate::buffer::{Buffer, BufferKind};
use crate::input::{Key, KeyCode};
use crate::mode::Mode;

/// Max gap (ms) between consecutive `<Esc>`es for the triple-`<Esc>` terminal escape
/// chord — three within this window of each other leave terminal mode. Matches the
/// `'mousetime'` default feel for "quick repeated taps."
const ESC_CHORD_WINDOW_MS: u64 = 500;

/// An action the core asks the server to perform on a terminal's PTY. Core can't
/// touch a process (it is pure/sync), so terminal lifecycle and input are enqueued
/// here and drained by the server with [`Editor::take_pending_terminal`] — the
/// terminal analogue of [`PendingOpen`](super::PendingOpen) /
/// [`PendingSave`](super::PendingSave).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOp {
    /// Spawn a PTY child for `buf`, sized `rows`×`cols`, running `argv` in `cwd`.
    /// An empty `argv` means the server's default shell; `cwd` `None` inherits the
    /// server's working directory. `scrollback` is the `'scrollback'` cap to give the
    /// emulator (rows kept after they scroll off the screen).
    Open {
        buf: BufferId,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
        scrollback: usize,
    },
    /// Forward input bytes (a translated keystroke or paste) to `buf`'s PTY child.
    Send { buf: BufferId, bytes: Vec<u8> },
    /// Kill `buf`'s PTY child and forget the session (the buffer was wiped, or the
    /// editor is shutting down).
    Kill { buf: BufferId },
}

impl Editor {
    /// `:terminal [cmd ...]` — open a terminal in the current window. With no
    /// argument the server spawns the default shell; otherwise the whitespace-split
    /// words are the program + args run directly (no shell). This is the builtin
    /// entry; the `nx.terminal` Lua control surface (programmatic open + keymaps)
    /// layers on top of [`Editor::open_terminal`].
    pub(crate) fn ex_terminal(&mut self, args: &str) {
        let argv: Vec<String> = args.split_whitespace().map(str::to_string).collect();
        self.open_terminal(argv, None);
    }

    /// The current window's text area as `(rows, cols)`, clamped to `u16` — the PTY
    /// winsize for a terminal shown there.
    pub fn current_text_area(&self) -> (u16, u16) {
        let rows = self.text_height().clamp(1, u16::MAX as usize) as u16;
        let cols = self.text_width().clamp(1, u16::MAX as usize) as u16;
        (rows, cols)
    }

    /// Open a terminal in the current window: create a fresh terminal-job buffer,
    /// switch to it, enter [`Mode::Terminal`], and enqueue a [`TerminalOp::Open`]
    /// sized to the current window's text area for the server to back with a PTY.
    /// `argv` empty ⇒ the server's default shell.
    pub fn open_terminal(&mut self, argv: Vec<String>, cwd: Option<String>) {
        let (rows, cols) = self.current_text_area();

        let mut buffer = Buffer::empty();
        buffer.kind = BufferKind::Terminal;
        // Seed the display name from the spawned command (the child replaces it with
        // its OSC window title once it sets one).
        buffer.terminal_title = Some(if argv.is_empty() {
            "shell".to_string()
        } else {
            argv.join(" ")
        });
        let buf = self.add_buffer(buffer);
        self.set_current_buffer(buf);
        self.mode = Mode::Terminal;
        self.terminal_pending_backslash = false;
        // A fresh emulator starts in normal cursor-key mode; the child re-enables
        // application mode (if it wants it) and the server mirrors that on the next project.
        self.terminal_app_cursor = false;
        self.pending_terminal.push(TerminalOp::Open {
            buf,
            argv,
            cwd,
            rows,
            cols,
            scrollback: self.options.scrollback,
        });
    }

    /// Drain the terminal actions queued this tick (called by the server's effect
    /// loop). See [`TerminalOp`].
    pub fn take_pending_terminal(&mut self) -> Vec<TerminalOp> {
        std::mem::take(&mut self.pending_terminal)
    }

    /// Send raw bytes to terminal buffer `buf`'s child — the same path a keystroke
    /// takes. Used by the server to write **query replies** (the cursor-position /
    /// device-attributes reports a real terminal answers automatically) back to the
    /// child so apps like fzf don't stall waiting for them. A no-op for a non-terminal
    /// buffer or empty input.
    pub fn terminal_send(&mut self, buf: BufferId, bytes: Vec<u8>) {
        if bytes.is_empty() || !self.is_terminal_buffer(buf) {
            return;
        }
        self.pending_terminal.push(TerminalOp::Send { buf, bytes });
    }

    /// Whether buffer `id` is a terminal-job buffer.
    pub fn is_terminal_buffer(&self, id: BufferId) -> bool {
        self.buffers
            .map
            .get(&id)
            .is_some_and(|ob| ob.buffer.is_terminal())
    }

    /// Buffer `id`'s terminal window title (the child's OSC title), or `None` if it
    /// is not a terminal buffer. Used as the buffer's reported name.
    pub fn terminal_title(&self, id: BufferId) -> Option<String> {
        self.buffers
            .map
            .get(&id)
            .filter(|ob| ob.buffer.is_terminal())
            .and_then(|ob| ob.buffer.terminal_title.clone())
    }

    /// Whether the current buffer accepts edits. A **live** terminal-job buffer is
    /// read-only: its lines mirror the child's screen (overwritten on every refresh),
    /// so only the child changes it — a `x`/`dd`/`p`/`R` would just corrupt the mirror
    /// until the next redraw. It becomes editable once the child exits (the `terminal`
    /// flag is cleared by [`Editor::terminal_closed`]). The edit chokepoints
    /// (`edit_each_cursor`, `apply_operator_to_range`, `paste`, …) consult this.
    pub(crate) fn modifiable(&self) -> bool {
        // Every non-ordinary buffer is read-only, enforced in *one* place: edits are
        // refused at the chokepoints with `E21`, exactly as vim treats a
        // `nomodifiable` buffer. [`Buffer::read_only`] covers the kinds carried as
        // buffer markers — a directory listing (the explorer / netrw), a plugin-owned
        // `nx.view`, a live terminal mirror, an image preview — and the quickfix /
        // loclist display buffers are added here (their identity is an `Editor`-side
        // registry, not a `Buffer` field). Consulting this at the chokepoints is what
        // closes the *ex-command* path — `:d`, `:s`, `:put`, `:normal` reach the edit
        // chokepoints, not the input router, so without it a `:d` would corrupt the
        // content even for a kind whose interactive keys are routed away. These
        // windows otherwise behave normally (motions, search, `<C-w>`, `:` all work).
        // The `'modifiable'` option is the third gate: an *ordinary* buffer the user (or
        // a built-in read-only scratch listing — `:messages`, `:registers`, …) flipped
        // `nomodifiable`, vim's plain per-buffer edit lock.
        !self.buffer().read_only() && !self.is_quickfix_buffer() && self.buffer().options.modifiable
    }

    /// Echo vim's `E21` when an edit is refused on a read-only (live terminal) buffer.
    pub(crate) fn refuse_edit(&mut self) {
        self.echo("E21: Cannot make changes, 'modifiable' is off".to_string());
    }

    /// Splice terminal buffer `buf`'s mirror: replace lines `[replace_from, end)`
    /// with `tail_lines` and place the terminal cursor at the absolute
    /// `(cursor_row, cursor_col)` (0-based). The buffer always mirrors the full
    /// `scrollback history ++ live screen`, so the cursor and line numbers are stable
    /// across `<C-\><C-n>` / `i` (like neovim). The server only **rewrites the live
    /// screen region** (`replace_from = history length`) on a refresh where nothing
    /// scrolled, and rebuilds from `replace_from = 0` only when the scrollback
    /// changed — so steady typing and most output stay cheap even with a full
    /// scrollback. Never touches undo or `modified` — a terminal buffer mirrors a
    /// live screen, not edited text. A no-op if `buf` is not an open terminal buffer.
    pub fn terminal_update(
        &mut self,
        buf: BufferId,
        replace_from: usize,
        tail_lines: &[String],
        cursor_row: usize,
        cursor_col: usize,
        app_cursor: bool,
    ) {
        if !self.is_terminal_buffer(buf) {
            return;
        }
        let mut text = tail_lines.join("\n");
        text.push('\n');
        let is_current = buf == self.cur_buffer();
        let ob = self.buffers.get_mut(buf);
        let from = replace_from.min(ob.buffer.line_count());
        let start = ob.buffer.line_start(from);
        let len = ob.buffer.len_bytes();
        ob.buffer.remove(start..len);
        ob.buffer.insert(start, &text);
        ob.buffer.normalize();
        // The whole rope was swapped, so the highlight/extmark layers must re-sync.
        ob.buffer.mark_resync();
        // A terminal buffer is never "modified" relative to a backing store — it has
        // none — and these refreshes must not flip the `[+]` flag or arm a write. This
        // must run *after* `mark_resync`, which sets `modified = true` (it can't tell a
        // live-screen mirror from an edit); resetting before it would be clobbered,
        // leaving a live terminal marked modified and blocking `:qa` with `E37`.
        ob.buffer.modified = false;

        if is_current {
            // Mirror the focused terminal's application-cursor-key mode so the next arrow /
            // Home / End keystroke is encoded in the form the child's terminfo expects.
            self.terminal_app_cursor = app_cursor;
            let line = cursor_row.min(self.buffer().line_count().saturating_sub(1));
            let line_text = self.buffer().line(line);
            // vt100 reports a *screen* column (display cells); the editor stores cursor
            // columns as *byte* offsets (the renderer re-projects byte→screen). Convert
            // here, or a multi-byte/wide glyph before the cursor — e.g. the `│` box-
            // drawing char a TUI's input box is framed with (3 bytes, 1 cell) — would
            // land the byte cursor mid-glyph and draw it cells too far left. Past the
            // last cell of the live line `byte_at_virtcol` returns `line_text.len()`,
            // keeping the next-write cursor just past end-of-line.
            let tab = self.buffer().options.effective_tabstop();
            let col = crate::unicode::byte_at_virtcol(&line_text, cursor_col, tab);
            // Stash the child's cursor so re-entering terminal mode (`i`/`a`) can snap
            // back to it. Move the *live* cursor only while in terminal-job mode — in
            // terminal-normal mode the user is navigating, so the child's output must
            // not yank their cursor away.
            self.terminal_cursor = (line, col);
            if self.mode == Mode::Terminal {
                self.cursor.line = line;
                self.cursor.col = col;
                self.clamp_cursor();
                self.ensure_visible();
            }
        }
    }

    /// Enter terminal-job mode from terminal-normal (`i`/`a`), snapping the cursor to
    /// the child's live input position (stashed by [`Editor::terminal_update`]) rather
    /// than leaving it where normal-mode navigation parked it.
    pub(crate) fn enter_terminal_mode(&mut self) {
        self.mode = Mode::Terminal;
        self.terminal_pending_backslash = false;
        self.terminal_esc_count = 0;
        let (line, col) = self.terminal_cursor;
        self.cursor.line = line;
        self.cursor.col = col;
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// Set terminal buffer `buf`'s display name to the child's window title (its OSC
    /// `\e]0;`/`\e]2;` sequence). Surfaced as the buffer name in the statusline. A
    /// no-op if `buf` is not an open terminal buffer or the title is empty.
    pub fn terminal_set_title(&mut self, buf: BufferId, title: &str) {
        if title.is_empty() || !self.is_terminal_buffer(buf) {
            return;
        }
        self.buffers.get_mut(buf).buffer.terminal_title = Some(title.to_string());
    }

    /// Mark terminal buffer `buf`'s child as exited with `code`: append a
    /// `[Process exited N]` notice and, if it is current and we're still in
    /// terminal mode, drop back to Normal so the dead buffer reads as plain text.
    /// A no-op if `buf` is not an open terminal buffer.
    pub fn terminal_closed(&mut self, buf: BufferId, code: i32) {
        if !self.is_terminal_buffer(buf) {
            return;
        }
        let notice = format!("\n[Process exited {code}]\n");
        let is_current = buf == self.cur_buffer();
        let ob = self.buffers.get_mut(buf);
        // Append the exit notice just before the phantom trailing newline.
        let at = ob.buffer.len_bytes().saturating_sub(1);
        ob.buffer.insert(at, &notice);
        ob.buffer.normalize();
        ob.buffer.mark_resync();
        // Keep the buffer **modified** (`mark_resync` set the flag, and we leave it):
        // unlike a *live* terminal — a screen mirror that clears the flag on every
        // refresh — an exited terminal's frozen output is unsaved content with no
        // backing file. Marking it modified makes `:q`/`:qa` warn (`E37`) before
        // discarding the child's output, so a command's results aren't lost silently;
        // it now behaves like any hand-edited unnamed scratch buffer.
        ob.buffer.modified = true;
        // The child is gone: clear the terminal flag so the buffer becomes an ordinary
        // (editable) scratch buffer holding the final output — keystrokes no longer
        // forward, and the read-only edit guard lifts.
        ob.buffer.kind = BufferKind::Ordinary;
        if is_current && self.mode == Mode::Terminal {
            self.mode = Mode::Normal;
            self.terminal_pending_backslash = false;
            self.terminal_esc_count = 0;
            self.clamp_cursor();
        }
    }

    /// Handle a keystroke while in [`Mode::Terminal`]: forward it to the child as
    /// input bytes, except `<C-\><C-n>` which leaves to Normal (terminal-normal
    /// mode). The `<C-\>` is held one keystroke (see
    /// [`terminal_pending_backslash`](Editor::terminal_pending_backslash)); if the
    /// next key isn't `<C-n>`, both the literal `<C-\>` (0x1c) and that key are sent.
    pub(crate) fn handle_terminal_key(&mut self, key: Key) {
        let buf = self.cur_buffer();
        // Awaiting the register name after `<C-\><C-r>` / `<C-S-r>`: this key names the
        // register whose text is typed into the child. (`<C-w>` = word under cursor,
        // matching insert mode's `<C-r><C-w>`.)
        if self.terminal_awaiting_register {
            self.terminal_awaiting_register = false;
            self.terminal_send_register(key);
            return;
        }
        if self.terminal_pending_backslash {
            self.terminal_pending_backslash = false;
            self.terminal_esc_count = 0;
            if key.ctrl && key.code == KeyCode::Char('n') {
                self.leave_terminal_mode();
                return;
            }
            // `<C-\><C-r>{reg}`: paste a register into the child. Behind the `<C-\>`
            // prefix so plain `<C-r>` still reaches the shell (reverse search).
            if key.ctrl && key.code == KeyCode::Char('r') {
                self.terminal_awaiting_register = true;
                return;
            }
            let mut bytes = vec![0x1c];
            bytes.extend(key_to_terminal_bytes(key, self.terminal_app_cursor));
            self.pending_terminal.push(TerminalOp::Send { buf, bytes });
            return;
        }
        // `<C-S-r>` arms register paste directly — the literal "Ctrl+Shift+R" — for any
        // client that can deliver `shift` on a ctrl-key (a legacy terminal cannot, so
        // `<C-\><C-r>` above is the portable spelling).
        if key.ctrl && key.shift && matches!(key.code, KeyCode::Char('r' | 'R')) {
            self.terminal_awaiting_register = true;
            return;
        }
        // The `<C-\>` escape prefix. Terminals deliver Ctrl-\ (the control byte 0x1c)
        // in two spellings: `<C-\>` proper, and — on macOS / xterm — `<C-4>` (crossterm
        // decodes 0x1c as Ctrl+'4' via the legacy control-code mapping). Accept both so
        // `<C-\><C-n>` works regardless of terminal.
        if key.ctrl && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4')) {
            self.terminal_pending_backslash = true;
            return;
        }
        // Triple-`<Esc>` is a discoverable escape hatch beside `<C-\><C-n>`: three in
        // *quick succession* (each within the chord window) leave to Normal, while the
        // first two `<Esc>`es are still forwarded to the child (so vim/htop inside keep
        // working). A slow `<Esc>` (gap past the window) restarts the run, so deliberate
        // single escapes a TUI program wants aren't hijacked. Any other key resets it.
        if key.code == KeyCode::Esc && !key.ctrl && !key.alt {
            let now = self.now_ms;
            let quick = self.terminal_esc_count > 0
                && now.saturating_sub(self.terminal_last_esc_ms) <= ESC_CHORD_WINDOW_MS;
            self.terminal_esc_count = if quick {
                self.terminal_esc_count + 1
            } else {
                1
            };
            self.terminal_last_esc_ms = now;
            if self.terminal_esc_count >= 3 {
                self.leave_terminal_mode();
                return;
            }
            self.pending_terminal.push(TerminalOp::Send {
                buf,
                bytes: vec![0x1b],
            });
            return;
        }
        self.terminal_esc_count = 0;
        let bytes = key_to_terminal_bytes(key, self.terminal_app_cursor);
        if !bytes.is_empty() {
            self.pending_terminal.push(TerminalOp::Send { buf, bytes });
        }
    }

    /// Type a register's text into the child (the key after `<C-\><C-r>` / `<C-S-r>`).
    /// `<C-w>` sends the word under the cursor; any other key names a register. A
    /// newline in the register becomes `\r` so a multi-line register runs like typed
    /// Enters. Unknown / empty registers send nothing.
    fn terminal_send_register(&mut self, key: Key) {
        let buf = self.cur_buffer();
        let text = if key.ctrl && key.code == KeyCode::Char('w') {
            self.word_under_cursor()
        } else if let Some(name) = key.as_char() {
            self.register_text(Some(name)).map(|(t, _)| t)
        } else {
            None
        };
        if let Some(text) = text {
            let bytes = text.replace('\n', "\r").into_bytes();
            if !bytes.is_empty() {
                self.pending_terminal.push(TerminalOp::Send { buf, bytes });
            }
        }
    }

    /// Leave terminal-job mode for terminal-normal (Normal on a terminal buffer),
    /// clearing the chord/esc-run state. Shared by `<C-\><C-n>` and triple-`<Esc>`.
    pub(crate) fn leave_terminal_mode(&mut self) {
        self.mode = Mode::Normal;
        self.terminal_pending_backslash = false;
        self.terminal_esc_count = 0;
        self.clamp_cursor();
    }
}

/// Translate one [`Key`] into the byte sequence a terminal child expects on its
/// PTY — printable text as UTF-8, the usual C0 control bytes, and the standard
/// `ESC [`-style sequences for the special keys. Pure: no editor state, so it is
/// trivially testable and identical across every front end.
pub(crate) fn key_to_terminal_bytes(key: Key, app_cursor: bool) -> Vec<u8> {
    // Alt/Meta sends ESC then the unmodified key's bytes (xterm's "metaSendsEscape").
    if key.alt {
        let mut inner = key_to_terminal_bytes(Key { alt: false, ..key }, app_cursor);
        if inner.is_empty() {
            return inner;
        }
        let mut bytes = vec![0x1b];
        bytes.append(&mut inner);
        return bytes;
    }
    // The cursor keys (arrows + Home/End) have two encodings, selected by the child's
    // DECCKM state: the default `\E[_` form, or — once a full-screen app enables
    // application cursor-key mode (`smkx` → `\E[?1h`) — the `\EO_` form its terminfo binds
    // (`kcuu1=\EOA`, `khome=\EOH`, `kend=\EOF`, …). Sending the wrong one makes the app
    // miss the key entirely: `less` reads `\E[H` as a stray `H` (its help command), `\E[F`
    // as `F` (tail-follow). The numeric-keypad keys (PageUp/Down, Delete) are `\E[_~`
    // sequences unaffected by DECCKM, so they stay below.
    let csi: u8 = if app_cursor { b'O' } else { b'[' };
    let cursor_seq = |last: u8| vec![0x1b, csi, last];
    match key.code {
        KeyCode::Up => return cursor_seq(b'A'),
        KeyCode::Down => return cursor_seq(b'B'),
        KeyCode::Right => return cursor_seq(b'C'),
        KeyCode::Left => return cursor_seq(b'D'),
        KeyCode::Home => return cursor_seq(b'H'),
        KeyCode::End => return cursor_seq(b'F'),
        _ => {}
    }
    match key.code {
        KeyCode::Char(c) => {
            if key.ctrl {
                // C0 control byte: Ctrl-A..Z → 1..26, and the punctuation controls
                // (Ctrl-@ → 0, Ctrl-[ → 27, Ctrl-\ → 28, Ctrl-] → 29, Ctrl-^ → 30,
                // Ctrl-_ → 31, Ctrl-Space → 0). For anything outside that range the
                // control modifier has no encoding, so send the bare char.
                let upper = c.to_ascii_uppercase();
                match upper {
                    '@'..='_' => vec![(upper as u8) & 0x1f],
                    // The legacy digit spellings of the C0 controls (xterm / crossterm
                    // decode the 0x1c..0x1f bytes as Ctrl+'4'..'7'): Ctrl-2 → NUL,
                    // Ctrl-3 → ESC, Ctrl-4 → FS, … Ctrl-8 → DEL.
                    ' ' | '2' => vec![0],
                    '3' => vec![0x1b],
                    '4' => vec![0x1c],
                    '5' => vec![0x1d],
                    '6' => vec![0x1e],
                    '7' | '/' => vec![0x1f],
                    '8' | '?' => vec![0x7f],
                    _ => c.to_string().into_bytes(),
                }
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        // Arrows + Home/End are handled above (DECCKM-aware). PageUp/Down are vt220 keypad
        // sequences unaffected by application cursor mode.
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => unreachable!("cursor keys handled by the DECCKM block above"),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        // A mouse key (`<LeftMouse>` / `<ScrollWheelUp>` …) is resolved server-side and
        // never reaches the terminal as input — it has no PTY byte encoding.
        KeyCode::Mouse { .. } | KeyCode::ScrollWheel(_) => Vec::new(),
    }
}
