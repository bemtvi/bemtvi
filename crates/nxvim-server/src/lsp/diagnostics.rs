//! Diagnostics: the per-buffer `publishDiagnostics` cache projected into the
//! redraw underline spans and the under-cursor message line, the
//! `:LspDiagnostics` location list, and `[d`/`]d` navigation.

use nxvim_core::unicode;
use nxvim_lsp::lsp_types::Diagnostic;
use nxvim_lsp::PositionEncoding;
use rmpv::Value;

use super::*;
use crate::redraw::StyleTable;
use crate::EditHost;

impl EditHost {
    /// The current buffer's cached diagnostics together with its server's
    /// negotiated position encoding, or `None` when the buffer has no attached
    /// server (so callers project nothing). Both borrows are released before any
    /// `&mut self` use.
    pub(crate) fn current_diagnostics(&self) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        self.diagnostics_of(self.editor.current_buffer_id())
    }

    /// Buffer-addressed form of [`EditHost::current_diagnostics`], for projecting a
    /// non-focused window's own buffer. Same `(diagnostics, encoding)` or `None`
    /// when that buffer has no attached server.
    pub(crate) fn diagnostics_of(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        let state = self.lsp_states.get(&buffer)?;
        let key = state.server.as_ref()?;
        let encoding = self.lsp_servers.get(key)?.encoding;
        Some((&state.diagnostics, encoding))
    }

    /// Build the per-row `diagnostics` redraw payload from a row→buffer-line
    /// mapping (`numbers`, 1-based, `None` for filler): each visible row's
    /// diagnostic underline spans as `[start_col, end_col, severity, style_id]`
    /// in **screen columns**. Mirrors [`EditHost::highlights_for`] — the LSP
    /// character offsets are converted to bytes through the negotiated encoding,
    /// then bytes to screen columns with the same tab/wide-char `virtcol` the
    /// highlights and selection use, so squiggles line up with the glyphs.
    /// `severity` is `1`=error … `4`=hint; `style_id` indexes the per-frame
    /// `styles` palette when the matching `DiagnosticUnderline*` group resolves
    /// through the registry (`Nil` otherwise, so the client falls back to a
    /// built-in severity color).
    /// Diagnostic counts for `buffer` by severity `[error, warn, info, hint]`,
    /// for the `diagnostics` statusline segment. Zero across the board when the
    /// buffer has no language server / no diagnostics.
    pub(crate) fn diag_counts_for(&self, buffer: nxvim_core::BufferId) -> [usize; 4] {
        let mut counts = [0usize; 4];
        if let Some((diags, _)) = self.diagnostics_of(buffer) {
            for d in diags {
                let sev = super::severity_code(d.severity); // 1=error … 4=hint
                if (1..=4).contains(&sev) {
                    counts[(sev - 1) as usize] += 1;
                }
            }
        }
        counts
    }

    pub(crate) fn diagnostics_for(
        &self,
        buffer: nxvim_core::BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        // `vim.diagnostic.config({ underline = false })` hides the squiggles; the
        // message line and the location list (other surfaces) are unaffected.
        let diags_encoding = if self.diag_config.underline {
            self.diagnostics_of(buffer)
        } else {
            None
        };
        let buf = self.editor.buffer_of(buffer);
        let Some((diags, encoding)) = diags_encoding else {
            // One empty entry per row so the client's `diagnostics[row]` index
            // stays aligned with `highlights`/`numbers`.
            return Value::Array(numbers.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        // Tab width is the rendered window's buffer's `tabstop` (it may differ
        // from the current buffer's), so the underline columns line up with the
        // text the client paints for that window.
        let tabstop = buf
            .map(|b| b.options.effective_tabstop())
            .unwrap_or(unicode::TABSTOP);
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(text) = buf.map(|b| b.line(line_idx)) else {
                    return Value::Array(Vec::new());
                };
                let spans = diags
                    .iter()
                    .filter_map(|d| {
                        let (start_byte, end_byte) =
                            self.diag_row_span(d, encoding, line_idx, &text)?;
                        let start_col = unicode::virtcol(&text, start_byte, tabstop);
                        let mut end_col = unicode::virtcol(&text, end_byte, tabstop);
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

    /// Build the per-row `diagnostics_virt` payload: for each visible row, the
    /// inline virtual-text decoration — the most severe diagnostic *starting* on
    /// that buffer line — as `[text, severity, style_id]`, or `Nil` when the row
    /// has none (or virtual text is off). `text` is the config prefix followed by
    /// the diagnostic's first message line; `severity` is `1`=error … `4`=hint;
    /// `style_id` indexes the per-frame `styles` palette when the matching
    /// `DiagnosticVirtualText*` group resolves (`Nil` otherwise, so the client
    /// falls back to a built-in severity color). Mirrors [`EditHost::diagnostics_for`]
    /// but emits one optional decoration per row rather than a span list — the text
    /// is positioned after end-of-line by the client, so no column conversion runs.
    pub(crate) fn diagnostics_virt_text_for(
        &self,
        buffer: nxvim_core::BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        let diags = if self.diag_config.virtual_text {
            self.diagnostics_of(buffer).map(|(d, _)| d)
        } else {
            None
        };
        let Some(diags) = diags else {
            // One `Nil` per row so the client's `diagnostics_virt[row]` index
            // stays aligned with `numbers`/`diagnostics`.
            return Value::Array(numbers.iter().map(|_| Value::Nil).collect());
        };
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Nil;
                };
                let line = (n - 1) as u32;
                // The most severe diagnostic that *starts* on this row wins the
                // line's one inline slot (ties broken by leftmost column).
                let best = diags
                    .iter()
                    .filter(|d| d.range.start.line == line)
                    .min_by_key(|d| (severity_code(d.severity), d.range.start.character));
                let Some(d) = best else {
                    return Value::Nil;
                };
                let severity = severity_code(d.severity);
                let text = format!("{}{}", self.diag_config.virt_prefix, first_line(&d.message));
                let style_id = match self
                    .editor
                    .highlights
                    .resolve(severity_virt_group(severity))
                {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                Value::Array(vec![
                    Value::from(text),
                    Value::from(severity as u64),
                    style_id,
                ])
            })
            .collect();
        Value::Array(rows)
    }

    /// Build the per-row `diagnostics_signs` payload: for each visible row, the
    /// gutter sign for the most severe diagnostic *starting* on that buffer line —
    /// as `[glyph, severity, style_id]`, or `Nil` when the row has none (or signs
    /// are off). `glyph` is the config (or built-in) per-severity letter; `severity`
    /// is `1`=error … `4`=hint; `style_id` indexes the per-frame `styles` palette
    /// when the matching `DiagnosticSign*` group resolves (`Nil` otherwise, so the
    /// client falls back to a built-in severity color). Mirrors
    /// [`EditHost::diagnostics_virt_text_for`] but addressed to the gutter.
    pub(crate) fn diagnostics_signs_for(
        &self,
        buffer: nxvim_core::BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        let diags = if self.diag_config.signs {
            self.diagnostics_of(buffer).map(|(d, _)| d)
        } else {
            None
        };
        let Some(diags) = diags else {
            // One `Nil` per row so the client's `diagnostics_signs[row]` index
            // stays aligned with `numbers`/`diagnostics`.
            return Value::Array(numbers.iter().map(|_| Value::Nil).collect());
        };
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Nil;
                };
                let line = (n - 1) as u32;
                // The most severe diagnostic that *starts* on this row wins the
                // line's sign cell (ties broken by leftmost column).
                let best = diags
                    .iter()
                    .filter(|d| d.range.start.line == line)
                    .min_by_key(|d| (severity_code(d.severity), d.range.start.character));
                let Some(d) = best else {
                    return Value::Nil;
                };
                let severity = severity_code(d.severity);
                let glyph = self.diag_config.sign_glyph(severity).to_string();
                let style_id = match self
                    .editor
                    .highlights
                    .resolve(severity_sign_group(severity))
                {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                Value::Array(vec![
                    Value::from(glyph),
                    Value::from(severity as u64),
                    style_id,
                ])
            })
            .collect();
        Value::Array(rows)
    }

    /// The rendered sign-column width in cells for a window, resolving its
    /// `'signcolumn'` policy against the signs actually present. Each sign column is
    /// 2 cells (vim). A diagnostic places at most one sign per line today, so the
    /// busiest visible line has 0 or 1 sign; the policy then decides the width:
    /// `no` → 0; `auto`/`auto:min-max` → 0 when no visible line has a sign, else
    /// `clamp(signs, min, max)` columns; `yes`/`yes:min-max` → `clamp(signs, min,
    /// max)` columns (so at least `min`, even on a clean buffer). When more sign
    /// sources arrive this widens automatically. Gated on `diag_config.signs`: with
    /// signs off, no diagnostic places a sign, so the busiest line has 0.
    pub(crate) fn sign_width_for(
        &self,
        buffer: nxvim_core::BufferId,
        numbers: &[Option<usize>],
        signcolumn: nxvim_core::SignColumn,
    ) -> u16 {
        use nxvim_core::SignColumn;
        if matches!(signcolumn, SignColumn::No) {
            return 0;
        }
        // The busiest visible line's sign count (0 or 1 today). A sign shows on a
        // visible numbered row when a diagnostic starts on that buffer line.
        let max_signs: u16 = if self.diag_config.signs {
            self.diagnostics_of(buffer).map_or(0, |(diags, _)| {
                let has = numbers.iter().flatten().any(|n| {
                    let line = (*n - 1) as u32;
                    diags.iter().any(|d| d.range.start.line == line)
                });
                u16::from(has)
            })
        } else {
            0
        };
        let cols = match signcolumn {
            SignColumn::No => 0,
            SignColumn::Auto { min, max } => {
                if max_signs == 0 {
                    0
                } else {
                    max_signs.clamp(min, max)
                }
            }
            // `clamp` lower-bounds at `min`, so `yes` always reserves its minimum.
            SignColumn::Yes { min, max } => max_signs.clamp(min, max),
        };
        cols * 2
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
    /// The current buffer's diagnostics as **location-list entries**
    /// `(path, line, col, text)` (0-based line/col), sorted by position — fed to
    /// [`nxvim_core::Editor::open_location_list`] by `:LspDiagnostics` /
    /// `vim.diagnostic.setloclist`. `None` when there are no diagnostics, or the
    /// buffer has no file path to navigate to.
    pub(crate) fn diagnostics_location_list(&self) -> Option<Vec<(PathBuf, usize, usize, String)>> {
        let (diags, encoding) = self.current_diagnostics()?;
        if diags.is_empty() {
            return None;
        }
        // A navigable list needs a file to jump into; a no-path buffer can't have one.
        let path = self.editor.buffer().path.clone()?;
        let mut items: Vec<&Diagnostic> = diags.iter().collect();
        items.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        let entries = items
            .into_iter()
            .map(|d| {
                let row = d.range.start.line as usize;
                let character = d.range.start.character as usize;
                let line = self.editor.buffer().line(row);
                let byte = byte_col(encoding, &line, character);
                let text = format!(
                    "{}: {}",
                    severity_short(severity_code(d.severity)),
                    first_line(&d.message),
                );
                (path.clone(), row, byte, text)
            })
            .collect();
        Some(entries)
    }

    /// `vim.diagnostic.open_float()`: open a float (the bottom panel, the same
    /// surface hover uses) listing every diagnostic on the cursor's line in full —
    /// the multi-line messages with their `source` and `code`, which the inline
    /// virtual text truncates to one line. Diagnostics are sorted by severity then
    /// start column; each is formatted as `E  source: message [code]`, its message
    /// split across as many panel rows as it has lines. A loud no-op (an echoed
    /// message, no panel) when the cursor's line has no diagnostics.
    pub(crate) fn diagnostics_open_float(&mut self) {
        // The cursor line's diagnostics: those *starting* on it (neovim's `lnum`
        // scope), matching the virt-text / sign surfaces. Collected and sorted
        // before any `&mut self` use so the borrow is released for `open_panel`.
        let row = self.editor.cursor.line as u32;
        let lines = match self.current_diagnostics() {
            Some((diags, _)) => {
                let mut items: Vec<&Diagnostic> =
                    diags.iter().filter(|d| d.range.start.line == row).collect();
                items.sort_by_key(|d| (severity_code(d.severity), d.range.start.character));
                items
                    .iter()
                    .flat_map(|d| diagnostic_float_lines(d))
                    .collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        if lines.is_empty() {
            self.editor.echo("No diagnostics under cursor");
            return;
        }
        self.editor.open_scratch_listing("[Diagnostics]", lines, 0);
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

/// Format one diagnostic as the panel rows `vim.diagnostic.open_float` shows: a
/// header `E  source: <first message line> [code]` followed by any remaining
/// message lines verbatim. Every line is control-sanitized like the single-line
/// surfaces ([`first_line`]) — the message text is untrusted server output, and
/// the panel paints it.
fn diagnostic_float_lines(d: &Diagnostic) -> Vec<String> {
    let mut msg_lines = d
        .message
        .lines()
        .map(sanitize_control)
        .filter(|l| !l.trim().is_empty());
    let mut header = format!("{}  ", severity_short(severity_code(d.severity)));
    if let Some(src) = d.source.as_deref().filter(|s| !s.is_empty()) {
        header.push_str(&sanitize_control(src));
        header.push_str(": ");
    }
    header.push_str(&msg_lines.next().unwrap_or_default());
    if let Some(code) = diagnostic_code(d) {
        header.push_str(&format!(" [{code}]"));
    }
    let mut out = vec![header];
    out.extend(msg_lines);
    out
}

/// A diagnostic's `code` rendered for the float header (a number stringified, a
/// string sanitized), or `None` when the server attached none.
fn diagnostic_code(d: &Diagnostic) -> Option<String> {
    use nxvim_lsp::lsp_types::NumberOrString;
    match d.code.as_ref()? {
        NumberOrString::Number(n) => Some(n.to_string()),
        NumberOrString::String(s) => Some(sanitize_control(s)),
    }
}

/// Strip terminal control characters from one line of (untrusted) server text,
/// the per-line half of [`first_line`]'s sanitizing — so a float row carrying a
/// multi-line message can't smuggle an escape sequence to the terminal.
fn sanitize_control(line: &str) -> String {
    line.chars().filter(|c| !c.is_control()).collect()
}
