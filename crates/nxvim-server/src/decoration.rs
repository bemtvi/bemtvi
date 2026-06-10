//! Driving Lua decoration providers each redraw frame.
//!
//! A decoration provider (`nvim_set_decoration_provider`) is a per-redraw callback
//! set: the server calls `on_start(tick)` once, then for every visible window
//! `on_win` and (unless `on_win` returned false) `on_line` per visible row, then
//! `on_end(tick)`. Inside `on_win` / `on_line` the provider places **ephemeral**
//! extmarks — single-frame highlights — which the server folds into a per-frame
//! store ([`Server::ephemeral_extmarks`]) that [`crate::extmarks`] reads while
//! projecting, then clears before the next frame. nvim-cmp uses this to highlight
//! the matched characters of each completion entry in its menu buffer.
//!
//! This runs *between* the view snapshot and the projection, in nxvim's "Lua
//! queues, server drains" model: the callbacks only read window/buffer state (via
//! the mirror) and queue ephemeral marks; they never mutate the editor mid-borrow.

use crate::Server;
use nxvim_core::view::View;

impl Server {
    /// Drive the registered decoration providers for this frame, rebuilding the
    /// per-frame ephemeral extmark store the projection then reads. Clears last
    /// frame's ephemeral marks first — unconditionally, so they never survive into
    /// a frame whose providers don't replace them — then returns early when no
    /// provider is registered, so the common frame pays only the (cheap) gate
    /// check.
    pub(crate) fn run_decoration_providers(&mut self, view: &View) {
        self.ephemeral_extmarks.clear();
        if !self.lua.has_decoration_providers().unwrap_or(false) {
            return;
        }

        // Providers read live window/buffer state through the Lua mirror (e.g. cmp
        // matches the window/buffer ids against its own float); refresh it first.
        self.push_buf_mirror();
        self.decor_tick += 1;
        let tick = self.decor_tick;
        if let Err(e) = self.lua.decor_frame_start(tick) {
            self.editor
                .echo(format!("E5108: decoration provider on_start: {e}"));
        }

        // The view's windows are parallel to `window_ids()` — both are the current
        // tab's leaves in layout order, then floats by z-order (see
        // `Editor::window_ids`) — so the i-th view window has the i-th id.
        let mut errors: Vec<String> = Vec::new();
        for (win, id) in view.windows.iter().zip(self.editor.window_ids()) {
            let Some((top, bot)) = visible_row_range(&win.numbers) else {
                continue; // an all-`~` window past the buffer end has no rows to decorate
            };
            match self
                .lua
                .decor_on_win(id.0, win.buffer.0, top as i64, bot as i64)
            {
                Ok(err) if !err.is_empty() => errors.push(err),
                Ok(_) => {}
                Err(e) => errors.push(e.to_string()),
            }
        }

        if let Err(e) = self.lua.decor_frame_end(tick) {
            self.editor
                .echo(format!("E5108: decoration provider on_end: {e}"));
        }

        // Fold the ephemeral marks the callbacks queued into the per-frame store
        // (each converted from 0-based (row, col) to byte offsets against the live
        // rope, then keyed by buffer) so the projection below sees them.
        for op in self.lua.take_ephemeral_extmark_ops() {
            self.apply_extmark_op(op);
        }
        for e in errors {
            self.editor.echo(format!("E5108: decoration provider: {e}"));
        }
    }
}

/// The first and last visible 0-based buffer rows of a window, from its per-row
/// `numbers` (1-based line numbers, `None` for `~` filler past the buffer end).
/// `None` when the window shows no real line (entirely filler). These are the
/// `(toprow, botrow)` neovim passes a provider's `on_win`, inclusive.
fn visible_row_range(numbers: &[Option<usize>]) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last = None;
    for n in numbers.iter().flatten() {
        let row = n - 1;
        first.get_or_insert(row);
        last = Some(row);
    }
    Some((first?, last?))
}
