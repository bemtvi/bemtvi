//! The server's leg of `foldmethod=expr` with a **generic** Lua `'foldexpr'`.
//!
//! nxvim-core owns the fold model but can't run Lua, so for any non-native
//! foldexpr (the tree-sitter and LSP markers are computed natively) the server
//! evaluates the expression per line — vim's `fold-expr` model, with `v:lnum`
//! bound — and pushes the per-line values back into the fold engine, which
//! resolves them to the fold structure (`set_foldexpr_values` →
//! `compute_generic_expr_folds`). Driven from [`EditHost::redraw`] before the view
//! is projected, and cached by `changedtick` so it re-evaluates only on an edit or
//! a foldexpr change (see [`Editor::pending_foldexpr`](nxvim_core::editor::Editor)).

use crate::EditHost;

impl EditHost {
    /// Evaluate the focused buffer's generic Lua `'foldexpr'` (when one is pending)
    /// and push the per-line fold values into the core fold engine. A no-op unless
    /// `foldmethod=expr` resolves to a generic foldexpr whose result for the current
    /// `changedtick` hasn't been computed yet.
    ///
    /// On an evaluation error the expression is surfaced on the message line and an
    /// empty result is stored, so a broken foldexpr fails loud once per edit rather
    /// than spinning (re-evaluating and re-echoing) every frame.
    pub(crate) fn refresh_expr_folds(&mut self) {
        let Some((buf, tick, expr, line_count)) = self.editor.pending_foldexpr() else {
            return;
        };
        // The foldexpr commonly reads buffer text (`vim.fn.getline(v:lnum)`); make
        // sure the Rust→Lua mirror reflects this frame before evaluating.
        self.push_buf_mirror();
        match self.lua.eval_foldexpr_lines(&expr, line_count) {
            Ok(values) => self.editor.set_foldexpr_values(buf, tick, values),
            Err(err) => {
                self.editor.set_foldexpr_values(buf, tick, Vec::new());
                self.editor.echo(format!("foldexpr: {err}"));
            }
        }
    }
}
