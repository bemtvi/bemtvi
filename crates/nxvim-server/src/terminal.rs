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

use nxvim_core::BufferId;

use crate::EditHost;

/// A terminal buffer's vt100 emulator: the escape-sequence parser (which owns the
/// screen grid) plus the last size it was projected at.
pub(crate) struct TermEmu {
    /// The vt100 parser + screen grid. Fed the child's PTY bytes; queried for the
    /// row text, cursor, and per-cell colors.
    parser: vt100::Parser,
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
            parser: vt100::Parser::new(rows, cols, 0),
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
    /// the screen into the buffer. A no-op if `buf` has no live emulator.
    pub fn terminal_feed(&mut self, buf: BufferId, bytes: &[u8]) {
        match self.terminals.get_mut(&buf) {
            Some(emu) => emu.parser.process(bytes),
            None => return,
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
        let (lines, cursor_row, cursor_col) = {
            let Some(emu) = self.terminals.get(&buf) else {
                return;
            };
            let screen = emu.parser.screen();
            let (_rows, cols) = screen.size();
            let lines: Vec<String> = screen.rows(0, cols).collect();
            let (cy, cx) = screen.cursor_position();
            (lines, cy as usize, cx as usize)
        };
        self.editor
            .terminal_update(buf, &lines, cursor_row, cursor_col);
    }
}
