//! Diagnostics: the per-buffer `publishDiagnostics` cache projected into the
//! redraw underline spans and the under-cursor message line, the
//! `:LspDiagnostics` location list, and `[d`/`]d` navigation.

use nxvim_core::unicode;
use nxvim_lsp::lsp_types::Diagnostic;
use nxvim_lsp::PositionEncoding;
use rmpv::Value;

use super::*;
use crate::redraw::StyleTable;
use crate::Server;

impl Server {
    /// The current buffer's cached diagnostics together with its server's
    /// negotiated position encoding, or `None` when the buffer has no attached
    /// server (so callers project nothing). Both borrows are released before any
    /// `&mut self` use.
    pub(crate) fn current_diagnostics(&self) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        let state = self.lsp_states.get(&self.editor.current_buffer_id())?;
        let key = state.server.as_ref()?;
        let encoding = self.lsp_servers.get(key)?.encoding;
        Some((&state.diagnostics, encoding))
    }

    /// Build the per-row `diagnostics` redraw payload from a row→buffer-line
    /// mapping (`numbers`, 1-based, `None` for filler): each visible row's
    /// diagnostic underline spans as `[start_col, end_col, severity, style_id]`
    /// in **screen columns**. Mirrors [`Server::highlights_for`] — the LSP
    /// character offsets are converted to bytes through the negotiated encoding,
    /// then bytes to screen columns with the same tab/wide-char `virtcol` the
    /// highlights and selection use, so squiggles line up with the glyphs.
    /// `severity` is `1`=error … `4`=hint; `style_id` indexes the per-frame
    /// `styles` palette when the matching `DiagnosticUnderline*` group resolves
    /// through the registry (`Nil` otherwise, so the client falls back to a
    /// built-in severity color).
    pub(crate) fn diagnostics_for(
        &self,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        // `vim.diagnostic.config({ underline = false })` hides the squiggles; the
        // message line and the location list (other surfaces) are unaffected.
        let diags_encoding = if self.diagnostics_underline {
            self.current_diagnostics()
        } else {
            None
        };
        let Some((diags, encoding)) = diags_encoding else {
            // One empty entry per row so the client's `diagnostics[row]` index
            // stays aligned with `highlights`/`numbers`.
            return Value::Array(numbers.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let text = self.editor.buffer().line(line_idx);
                let spans = diags
                    .iter()
                    .filter_map(|d| {
                        let (start_byte, end_byte) =
                            self.diag_row_span(d, encoding, line_idx, &text)?;
                        let start_col = unicode::virtcol(&text, start_byte, unicode::TABSTOP);
                        let mut end_col = unicode::virtcol(&text, end_byte, unicode::TABSTOP);
                        // A zero-width range (e.g. an empty span at end-of-line)
                        // still needs one underlined cell to be visible.
                        if end_col <= start_col {
                            end_col = start_col + 1;
                        }
                        let severity = severity_code(d.severity);
                        let style_id =
                            match self.editor.highlights.resolve(severity_group(severity)) {
                                Some(style) => Value::from(styles.intern(style) as u64),
                                None => Value::Nil,
                            };
                        Some(Value::Array(vec![
                            Value::from(start_col as u64),
                            Value::from(end_col as u64),
                            Value::from(severity as u64),
                            style_id,
                        ]))
                    })
                    .collect();
                Value::Array(spans)
            })
            .collect();
        Value::Array(rows)
    }

    /// The message of the highest-severity diagnostic whose range covers the
    /// cursor, for the message line (shown only when no other message is set, so
    /// `:messages` history stays clean). `None` when the cursor is on no
    /// diagnostic. Newlines are flattened so it fits one line.
    pub(crate) fn diagnostic_under_cursor(&self) -> Option<String> {
        let (diags, encoding) = self.current_diagnostics()?;
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        diags
            .iter()
            .filter(|d| {
                self.diag_row_span(d, encoding, row, &line)
                    // Cover the resting cell of a zero-width range too.
                    .is_some_and(|(s, e)| col >= s && col < e.max(s + 1))
            })
            .min_by_key(|d| severity_code(d.severity))
            .map(|d| first_line(&d.message))
    }

    /// The `[start, end)` **byte** span a diagnostic occupies on buffer row
    /// `line_idx` (whose text is `line`), or `None` if it does not reach that
    /// row. Multi-line ends are clipped to the row: `0` before the range's first
    /// line, the line length after its last. The LSP character offsets are
    /// converted to bytes through the negotiated `encoding` (Decision 4).
    pub(crate) fn diag_row_span(
        &self,
        d: &Diagnostic,
        encoding: PositionEncoding,
        line_idx: usize,
        line: &str,
    ) -> Option<(usize, usize)> {
        let (s, e) = (d.range.start, d.range.end);
        let row = line_idx as u32;
        if row < s.line || row > e.line {
            return None;
        }
        let start = if s.line == row {
            byte_col(encoding, line, s.character as usize)
        } else {
            0
        };
        let end = if e.line == row {
            byte_col(encoding, line, e.character as usize)
        } else {
            line.len()
        };
        Some((start, end))
    }

    /// Build the `:LspDiagnostics` location list for the current buffer: one
    /// `severity  line:col  message` row per diagnostic (sorted by position) and
    /// a parallel [`PanelTarget`] list to attach as the panel's jump targets.
    /// `None` when the buffer has no diagnostics.
    pub(crate) fn diagnostics_location_list(&self) -> Option<(Vec<String>, Vec<PanelTarget>)> {
        let (diags, encoding) = self.current_diagnostics()?;
        if diags.is_empty() {
            return None;
        }
        let path = self.editor.buffer().path.clone();
        let mut items: Vec<&Diagnostic> = diags.iter().collect();
        items.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        let mut lines = Vec::with_capacity(items.len());
        let mut targets = Vec::with_capacity(items.len());
        for d in items {
            let row = d.range.start.line as usize;
            let character = d.range.start.character as usize;
            lines.push(format!(
                "{}  {}:{}  {}",
                severity_short(severity_code(d.severity)),
                row + 1,
                character + 1,
                first_line(&d.message),
            ));
            let line = self.editor.buffer().line(row);
            let byte = byte_col(encoding, &line, character);
            targets.push(path.clone().map(|p| (p, row, byte)));
        }
        Some((lines, targets))
    }

    /// `vim.diagnostic.goto_next`/`goto_prev`: move the cursor to the next
    /// (`forward`) or previous diagnostic in the current buffer, wrapping around
    /// the ends. `severity` (1=ERROR…4=HINT) restricts the set when set. A no-op
    /// when the buffer has no (matching) diagnostics. Reuses the same byte-column
    /// conversion the underline path uses, then `jump_to`s the *current* file so
    /// the move snaps to a valid resting cell (no file open — same buffer).
    pub(crate) fn diagnostic_goto(&mut self, forward: bool, severity: Option<u8>) {
        let Some((diags, encoding)) = self.current_diagnostics() else {
            return;
        };
        // Resolve every (matching) diagnostic to a 0-based (line, byte col) and
        // sort by position, so "next/previous from the cursor" is a list walk.
        let mut positions: Vec<(usize, usize)> = diags
            .iter()
            .filter(|d| severity.map_or(true, |s| severity_code(d.severity) == s))
            .map(|d| {
                let row = d.range.start.line as usize;
                let line = self.editor.buffer().line(row);
                (
                    row,
                    byte_col(encoding, &line, d.range.start.character as usize),
                )
            })
            .collect();
        if positions.is_empty() {
            return;
        }
        positions.sort_unstable();
        positions.dedup();

        let cur = (self.editor.cursor.line, self.editor.cursor.col);
        // The next strictly-after (forward) or strictly-before (backward) target,
        // wrapping to the first/last when the cursor is past the last/before the
        // first — neovim's `goto_next`/`goto_prev` wrap behavior.
        let target = if forward {
            positions
                .iter()
                .find(|&&p| p > cur)
                .copied()
                .unwrap_or(positions[0])
        } else {
            positions
                .iter()
                .rev()
                .find(|&&p| p < cur)
                .copied()
                .unwrap_or(positions[positions.len() - 1])
        };

        let (line, byte) = target;
        if let Some(path) = self.editor.buffer().path.clone() {
            self.editor.jump_to(&path, line, byte);
        }
    }

    /// The cached diagnostics whose range covers the cursor, cloned as the
    /// `context.diagnostics` for a code-action request (empty when none / no
    /// server). They are already in the server's negotiated encoding, as the
    /// server sent them.
    pub(crate) fn diagnostics_at_cursor(&self) -> Vec<Diagnostic> {
        let Some((diags, encoding)) = self.current_diagnostics() else {
            return Vec::new();
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        diags
            .iter()
            .filter(|d| {
                self.diag_row_span(d, encoding, row, &line)
                    .is_some_and(|(s, e)| col >= s && col < e.max(s + 1))
            })
            .cloned()
            .collect()
    }
}
