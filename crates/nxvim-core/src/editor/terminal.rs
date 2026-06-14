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
use crate::buffer::Buffer;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;

/// An action the core asks the server to perform on a terminal's PTY. Core can't
/// touch a process (it is pure/sync), so terminal lifecycle and input are enqueued
/// here and drained by the server with [`Editor::take_pending_terminal`] — the
/// terminal analogue of [`PendingOpen`](super::PendingOpen) /
/// [`PendingSave`](super::PendingSave).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOp {
    /// Spawn a PTY child for `buf`, sized `rows`×`cols`, running `argv` in `cwd`.
    /// An empty `argv` means the server's default shell; `cwd` `None` inherits the
    /// server's working directory.
    Open {
        buf: BufferId,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
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
        buffer.terminal = true;
        let buf = self.add_buffer(buffer);
        self.set_current_buffer(buf);
        self.mode = Mode::Terminal;
        self.terminal_pending_backslash = false;
        self.pending_terminal.push(TerminalOp::Open {
            buf,
            argv,
            cwd,
            rows,
            cols,
        });
    }

    /// Drain the terminal actions queued this tick (called by the server's effect
    /// loop). See [`TerminalOp`].
    pub fn take_pending_terminal(&mut self) -> Vec<TerminalOp> {
        std::mem::take(&mut self.pending_terminal)
    }

    /// Whether buffer `id` is a terminal-job buffer.
    pub fn is_terminal_buffer(&self, id: BufferId) -> bool {
        self.buffers
            .map
            .get(&id)
            .is_some_and(|ob| ob.buffer.terminal)
    }

    /// Replace terminal buffer `buf`'s mirrored screen with `lines` and place the
    /// terminal cursor at `(cursor_row, cursor_col)` (0-based, in the new lines).
    /// Pushed in by the server's terminal engine on each PTY output update. Never
    /// touches undo or `modified` — a terminal buffer mirrors a live screen, it is
    /// not edited text. A no-op if `buf` is not an open terminal buffer.
    pub fn terminal_update(
        &mut self,
        buf: BufferId,
        lines: &[String],
        cursor_row: usize,
        cursor_col: usize,
    ) {
        if !self.is_terminal_buffer(buf) {
            return;
        }
        let mut text = lines.join("\n");
        text.push('\n');
        let is_current = buf == self.cur_buffer();
        let ob = self.buffers.get_mut(buf);
        let len = ob.buffer.len_bytes();
        ob.buffer.remove(0..len);
        ob.buffer.insert(0, &text);
        ob.buffer.normalize();
        // A terminal buffer is never "modified" relative to a backing store — it has
        // none — and these refreshes must not flip the `[+]` flag or arm a write.
        ob.buffer.modified = false;
        // The whole rope was swapped, so the highlight/extmark layers must re-sync
        // (the projector skips treesitter for terminal buffers, but extmark anchors
        // would otherwise dangle on stale byte offsets).
        ob.buffer.mark_resync();

        if is_current {
            let line = cursor_row.min(self.buffer().line_count().saturating_sub(1));
            let line_text = self.buffer().line(line);
            // vt100 reports a screen column; for the common ASCII case it maps to a
            // byte column. Clamp into the line so a wide-char / trailing-cell cursor
            // never lands past the text. (Wide-char column fidelity is deferred.)
            let col = cursor_col.min(line_text.len());
            self.cursor.line = line;
            self.cursor.col = col;
            self.clamp_cursor();
            self.ensure_visible();
        }
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
        ob.buffer.modified = false;
        ob.buffer.mark_resync();
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
        if self.terminal_pending_backslash {
            self.terminal_pending_backslash = false;
            self.terminal_esc_count = 0;
            if key.ctrl && key.code == KeyCode::Char('n') {
                self.leave_terminal_mode();
                return;
            }
            let mut bytes = vec![0x1c];
            bytes.extend(key_to_terminal_bytes(key));
            self.pending_terminal.push(TerminalOp::Send { buf, bytes });
            return;
        }
        if key.ctrl && key.code == KeyCode::Char('\\') {
            self.terminal_pending_backslash = true;
            return;
        }
        // Triple-`<Esc>` is a discoverable escape hatch beside `<C-\><C-n>`: the first
        // two `<Esc>`es are still forwarded to the child (so vim/htop inside keep
        // working), the third leaves to Normal. Any other key resets the run.
        if key.code == KeyCode::Esc && !key.ctrl && !key.alt {
            self.terminal_esc_count += 1;
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
        let bytes = key_to_terminal_bytes(key);
        if !bytes.is_empty() {
            self.pending_terminal.push(TerminalOp::Send { buf, bytes });
        }
    }

    /// Leave terminal-job mode for terminal-normal (Normal on a terminal buffer),
    /// clearing the chord/esc-run state. Shared by `<C-\><C-n>` and triple-`<Esc>`.
    fn leave_terminal_mode(&mut self) {
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
pub(crate) fn key_to_terminal_bytes(key: Key) -> Vec<u8> {
    // Alt/Meta sends ESC then the unmodified key's bytes (xterm's "metaSendsEscape").
    if key.alt {
        let mut inner = key_to_terminal_bytes(Key { alt: false, ..key });
        if inner.is_empty() {
            return inner;
        }
        let mut bytes = vec![0x1b];
        bytes.append(&mut inner);
        return bytes;
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
                    ' ' => vec![0],
                    '?' => vec![0x7f],
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
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
    }
}
