//! Draining queued side effects to convergence: applying the Lua chunk's
//! highlights/commands/output/panel/LSP/loop/buffer ops, the Rust→Lua buffer
//! mirror, event-loop completions, and the `run_pending` fixpoint.

#[cfg(feature = "native")]
use crate::evloop::{LoopCommand, LoopEvent};
#[cfg(feature = "native")]
use crate::lsp::CODE_ACTION_PANEL_TITLE;
use crate::EditHost;
use nxvim_core::highlight::HlDef;
use nxvim_core::{
    parse_color, BorderStyle, BufferId, FloatAnchor, FloatConfig, FloatRelative, TabId, UndoEntry,
    UndoTreeView, WindowConfigSpec, WindowId,
};
use nxvim_lua::{
    BoMirror, BufBytesEdit, BufMirror, BufOp, CallbackArgs, ExtmarkMirror, ExtmarkOp, FloatMirror,
    GoMirror, HlDefMirror, HlSet, JumpMirror, LoopOp, OptionValue, PanelOp, TabMirror, TabOp, TsOp,
    WindowMirror, WindowOp,
};
use rmpv::Value;
use std::collections::HashSet;

/// Byte offset of a neovim 0-based `(row, col)` position in `buf`, clamped into
/// the buffer (row into `[0, line_count]`, col into the line's byte length) the
/// way neovim tolerates out-of-range extmark positions. `col` is a byte offset
/// within the line, matching the rest of nxvim's column model.
fn byte_of(buf: &nxvim_core::Buffer, row: i64, col: i64) -> usize {
    let n = buf.line_count();
    let row = (row.max(0) as usize).min(n);
    let line_len = if row < n { buf.line(row).len() } else { 0 };
    let col = (col.max(0) as usize).min(line_len);
    buf.line_start(row) + col
}

/// neovim 0-based `(row, col)` of byte offset `byte` in `buf` — the inverse of
/// [`byte_of`], for projecting stored extmark anchors back into the Lua mirror.
/// `col` is a byte offset within the line.
fn byte_rowcol(buf: &nxvim_core::Buffer, byte: usize) -> (u64, u64) {
    let byte = byte.min(buf.len_bytes());
    let row = buf.byte_to_line(byte);
    let col = byte - buf.line_start(row);
    (row as u64, col as u64)
}

/// Project a core [`BufferEdit`] (absolute byte offsets + `(row, byte-col)`
/// points) into neovim's `on_bytes` argument tuple, whose row/col fields are
/// *relative* deltas: `start_*` is the absolute start, the `old_*`/`new_*` triples
/// are `(rows spanned, col on the last spanned row, byte count)`. This is the
/// inverse of the vendored `LanguageTree:_on_bytes` reconstruction
/// (`old_end_col = old_col + (old_row == 0 ? start_col : 0)`, etc.), so a round
/// trip through it recovers the original absolute edit.
fn on_bytes_edit(bufnr: u64, tick: u64, e: &nxvim_core::BufferEdit) -> BufBytesEdit {
    let (sr, sc) = e.start_point;
    let (or_, oc) = e.old_end_point;
    let (nr, nc) = e.new_end_point;
    let old_row = or_ - sr;
    let new_row = nr - sr;
    BufBytesEdit {
        bufnr,
        tick,
        start_row: sr as u64,
        start_col: sc as u64,
        start_byte: e.start_byte as u64,
        old_row: old_row as u64,
        // On the same row as the start, the column the deleted/inserted region ends
        // at is relative to `start_col`; spanning rows, it's the absolute column on
        // the last row (matching `_on_bytes`'s `old_end_col` reconstruction).
        old_col: (if old_row == 0 { oc - sc } else { oc }) as u64,
        old_byte: (e.old_end_byte - e.start_byte) as u64,
        new_row: new_row as u64,
        new_col: (if new_row == 0 { nc - sc } else { nc }) as u64,
        new_byte: (e.new_end_byte - e.start_byte) as u64,
    }
}

/// Serialize a [`UndoTreeView`] into the msgpack map `vim.fn.undotree()` returns.
fn undotree_value(v: &UndoTreeView) -> Value {
    let entries: Vec<Value> = v.entries.iter().map(undo_entry_value).collect();
    Value::Map(vec![
        (Value::from("synced"), Value::from(1)),
        (Value::from("seq_last"), Value::from(v.seq_last)),
        (Value::from("seq_cur"), Value::from(v.seq_cur)),
        (Value::from("save_last"), Value::from(v.save_last)),
        (Value::from("save_cur"), Value::from(v.save_cur)),
        (Value::from("time_cur"), Value::from(v.time_cur)),
        (Value::from("entries"), Value::Array(entries)),
    ])
}

/// Serialize one [`UndoEntry`] (recursively, including its `alt` branches).
fn undo_entry_value(e: &UndoEntry) -> Value {
    let mut map = vec![
        (Value::from("seq"), Value::from(e.seq)),
        (Value::from("time"), Value::from(e.time)),
    ];
    if let Some(s) = e.save {
        map.push((Value::from("save"), Value::from(s)));
    }
    if !e.alt.is_empty() {
        let alt: Vec<Value> = e.alt.iter().map(undo_entry_value).collect();
        map.push((Value::from("alt"), Value::Array(alt)));
    }
    Value::Map(map)
}

/// Translate a core [`FloatConfig`] into the [`FloatMirror`] the `nx._wins`
/// mirror carries — the enums become the strings `nvim_win_get_config` returns,
/// so nxvim-lua never sees the core's float types. The inverse of the
/// `parse_float_config` / `WindowOp::OpenFloat` parse.
fn float_mirror(cfg: FloatConfig) -> FloatMirror {
    let (relative, win) = match cfg.relative {
        FloatRelative::Editor => ("editor", 0),
        FloatRelative::Cursor => ("cursor", 0),
        FloatRelative::Win(id) => ("win", id.0),
    };
    FloatMirror {
        relative: relative.to_string(),
        win,
        anchor: cfg.anchor.as_str().to_string(),
        row: cfg.row as i64,
        col: cfg.col as i64,
        width: cfg.width as u64,
        height: cfg.height as u64,
        zindex: cfg.zindex as u64,
        focusable: cfg.focusable,
        border: cfg.border.as_str().to_string(),
        title: cfg.title,
    }
}

impl EditHost {
    /// Apply the side effects the last Lua chunk left in the runtime: highlight
    /// definitions fold into the core registry, queued ex-commands run against
    /// the editor, and the final captured `print` / `nvim_echo` line becomes the
    /// message.
    pub(crate) fn apply_lua_effects(&mut self) {
        for hl in self.lua.take_highlights() {
            self.editor.highlights.set_ns(hl.ns, &hl.name, hl_def(&hl));
        }
        for cmd in self.lua.take_commands() {
            self.editor.command(&cmd);
        }
        // Each captured `print` / `nvim_echo` line becomes a message: the last
        // is shown on the message line, and every line lands in `:messages`.
        for line in self.lua.take_output() {
            self.editor.echo(line);
        }
        // Panel requests from `vim.panel.*` drive the core's panel state.
        for op in self.lua.take_panel_ops() {
            match op {
                PanelOp::Open {
                    title,
                    lines,
                    wants_select,
                    cursor,
                } => {
                    self.editor.open_panel(title, lines, wants_select, cursor);
                }
                PanelOp::SetLines(lines) => self.editor.set_panel_lines(lines),
                PanelOp::OnSelect(wants) => self.editor.set_panel_on_select(wants),
                PanelOp::SetCursor(line) => self.editor.set_panel_cursor(line),
                PanelOp::Close => self.editor.close_panel(),
            }
        }
        // Server-start requests from `vim.lsp.start` (the `vim.lsp.enable` FileType
        // dispatcher) bind a buffer to its language server and ensure it is spawned.
        // Native only — a serverless browser build has no language servers (Phase 6);
        // a config that tries fails *loud* rather than silently dropping the request.
        #[cfg(feature = "native")]
        for op in self.lua.take_lsp_ops() {
            self.apply_lsp_op(op);
        }
        #[cfg(not(feature = "native"))]
        if !self.lua.take_lsp_ops().is_empty() {
            self.editor
                .echo("E: language servers (vim.lsp) are not available in the browser build yet");
        }
        // Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `vim.uv`
        // timers / async `vim.system`: a `Schedule` is serviced directly (queued
        // for the trailing `run_pending` drain); everything else is forwarded to
        // the background event-loop actor, whose completions arrive on the
        // `loop_events` `select!` arm.
        for op in self.lua.take_loop_ops() {
            self.apply_loop_op(op);
        }
        // Buffer-local option writes (`vim.bo`): applied to the live editor after the
        // chunk, catching the core up with the write-through the Lua side already did
        // against its option mirror. (Buffer *text* / lifecycle mutation is not part of
        // the Lua API — see `BufOp`.)
        for op in self.lua.take_buf_ops() {
            self.apply_buf_op(op);
        }
        // Extmark mutations from the `nvim_buf_set_extmark` family (the decoration
        // layer): applied to the target buffer's `ExtmarkStore` after the chunk,
        // catching the core up with the write-through the Lua side did against its
        // `nx._extmarks` mirror.
        for op in self.lua.take_extmark_ops() {
            self.apply_extmark_op(op);
        }
        // Window mutations from the `vim.api.nvim_win_*` family (Phase 5): applied
        // to the live editor after the chunk. Their `WinNew`/`WinEnter`/… autocmds
        // fire from `emit_lifecycle_events`, which `run_pending` runs once the
        // ops have settled.
        for op in self.lua.take_window_ops() {
            self.apply_window_op(op);
        }
        // Tab-page mutations from `nvim_set_current_tabpage` (Phase 3): applied to
        // the live editor after the chunk. Their `TabLeave`/`TabEnter`/… autocmds
        // fire from `emit_lifecycle_events`, run once the ops have settled.
        for op in self.lua.take_tab_ops() {
            self.apply_tab_op(op);
        }
        // Global-option writes from `vim.o` (a wired search option): applied to the
        // editor's global options after the chunk — the same state the `:set` ex
        // path writes. The booleans are the search flags; numeric
        // `showtabline`/`laststatus` and the `statusline` string are wired too.
        for op in self.lua.take_global_ops() {
            match op.value {
                OptionValue::Bool(b) => self.editor.set_global_option_bool(&op.name, b),
                // `showtabline` is the one wired numeric global (the Lua side
                // forwards the search booleans and `showtabline`).
                OptionValue::Number(n) => self.editor.set_global_option_num(&op.name, n),
                // `statusline` (the one wired string global) — same home the
                // `:set statusline=…` ex path writes.
                OptionValue::String(s) => self.editor.set_global_option_str(&op.name, &s),
            }
        }
        // Treesitter bridges from `vim.treesitter.*`: the per-buffer start/stop
        // toggle (ADR 0001, #1) and the query-resolution push (#4). Each ends by
        // dropping the affected highlight memo(s) so the next redraw re-queries the
        // engine — the change isn't reflected in any buffer's changedtick.
        // Native only — the in-process treesitter engine isn't built for wasm (the
        // browser highlights JS-side in `nxvim-edithost`); a `vim.treesitter.*` op fails loud.
        #[cfg(feature = "native")]
        for op in self.lua.take_ts_ops() {
            match op {
                TsOp::SetQuery { lang, name, text } => {
                    // `nx.treesitter.set_query`: install the override on the engine
                    // directly — no Lua merge/resolution. A compile failure echoes
                    // loud via `set_ts_query` itself. A query change is rare (config
                    // time) and lang-wide, so drop every buffer's highlight memo
                    // rather than track which are this language; they all re-query on
                    // the next redraw.
                    self.editor.set_ts_query(&lang, &name, text);
                    self.syntax_states.clear();
                }
            }
        }
        #[cfg(not(feature = "native"))]
        if !self.lua.take_ts_ops().is_empty() {
            self.editor
                .echo("E: vim.treesitter is not available in the browser build yet");
        }
        // Register writes from `vim.fn.setreg`: applied to the editor's register
        // file after the chunk — the same store yanks/deletes write. The Lua side
        // already rejected read-only specials and resolved uppercase/`a` append.
        for op in self.lua.take_reg_ops() {
            self.editor
                .set_register_api(op.name, op.text, op.linewise, op.append);
        }
        // `vim.ui.input` prompts (Phase 8): open the editor's command line as a
        // labelled text prompt and remember which callback awaits the result. Only
        // one prompt can be open at a time (a single command line); if several were
        // queued, the last wins (its label/default is what shows) — a documented
        // single-prompt limitation, not a silent drop.
        for req in self.lua.take_ui_inputs() {
            self.editor.open_prompt(req.prompt, req.default);
            self.pending_ui_input = Some(req.cb_id);
        }
        // `vim.fn.confirm` button dialogs: open the command line as a single-key
        // confirm prompt and remember the callback that resumes the blocked
        // `vim.fn.confirm` coroutine. Shares the `pending_ui_input` slot and the
        // `prompt_results` channel with `vim.ui.input` (only one prompt is open at
        // a time); the chosen index arrives as a string the Lua side reads back.
        for req in self.lua.take_confirms() {
            self.editor
                .open_confirm(req.label, req.accelerators, req.default);
            self.pending_ui_input = Some(req.cb_id);
        }
        // `nvim_feedkeys` typeahead: parse each request's keys and queue them onto
        // the server's feed buffer (the front for an `i` insert, else the back),
        // carrying the remap flag. The buffer is drained — fed through the matcher
        // or straight to the editor — by `drain_feedkeys` at the batch / settle
        // boundary, never re-entrantly here.
        for op in self.lua.take_feedkeys() {
            let keys = nxvim_core::parse_keys(&op.keys);
            if op.insert {
                // Insert at the front while preserving the keys' own order.
                for key in keys.into_iter().rev() {
                    self.feed_buffer.push_front((key, op.remap));
                }
            } else {
                for key in keys {
                    self.feed_buffer.push_back((key, op.remap));
                }
            }
        }
    }

    /// Apply one [`BufOp`] to the live editor: a buffer-local option write (`vim.bo` /
    /// `nvim_set_option_value` with a `buf`). The buffer-*text* / lifecycle mutation
    /// surface (`nvim_buf_set_lines`, `nvim_create_buf`, `nvim_buf_delete`) is
    /// intentionally absent from nxvim's Lua API (see `nxvim-lua`'s `prelude/api.lua`
    /// header), so this is option-only.
    pub(crate) fn apply_buf_op(&mut self, op: BufOp) {
        match op {
            BufOp::SetOption { bufnr, name, value } => {
                let id = BufferId(bufnr);
                match value {
                    OptionValue::Number(n) => self.editor.set_buffer_option_num(id, &name, n),
                    OptionValue::Bool(b) => self.editor.set_buffer_option_bool(id, &name, b),
                    // `regexsyntax` is the one wired buffer-local string option
                    // (`vim.bo.regexsyntax`); the `_buf_set_option` bridge forwards
                    // it as a `String`.
                    OptionValue::String(s) => self.editor.set_buffer_option_str(id, &name, &s),
                }
            }
        }
    }

    /// Apply one [`ExtmarkOp`] to the target buffer's `ExtmarkStore`, converting
    /// the neovim 0-based `(row, col)` positions to byte offsets against the live
    /// rope (the conversion the Lua side can't do without the text). Positions are
    /// clamped into the buffer, matching neovim's tolerance for out-of-range
    /// marks. A missing buffer is a no-op (it was deleted between queue and drain).
    pub(crate) fn apply_extmark_op(&mut self, op: ExtmarkOp) {
        match op {
            ExtmarkOp::Set {
                bufnr,
                ns,
                id,
                row,
                col,
                end_row,
                end_col,
                hl_group,
                priority,
            } => {
                let bid = BufferId(bufnr);
                let Some(buf) = self.editor.buffer_of(bid) else {
                    return;
                };
                let start = byte_of(buf, row, col);
                let end = match (end_row, end_col) {
                    (Some(r), Some(c)) => Some(byte_of(buf, r, c).max(start)),
                    _ => None,
                };
                if let Some(buf) = self.editor.buffer_of_mut(bid) {
                    buf.extmarks
                        .set(ns, Some(id), start, end, hl_group, priority);
                }
            }
            ExtmarkOp::Del { bufnr, ns, id } => {
                if let Some(buf) = self.editor.buffer_of_mut(BufferId(bufnr)) {
                    buf.extmarks.del(ns, id);
                }
            }
            ExtmarkOp::Clear {
                bufnr,
                ns,
                line_start,
                line_end,
            } => {
                let bid = BufferId(bufnr);
                let Some(buf) = self.editor.buffer_of(bid) else {
                    return;
                };
                // `(0, -1)` ⇒ the whole buffer (clear every mark in the namespace);
                // any narrower range clips to the spanned bytes.
                let range = if line_start <= 0 && line_end == -1 {
                    None
                } else {
                    let n = buf.line_count();
                    let ls = (line_start.max(0) as usize).min(n);
                    let start = buf.line_start(ls);
                    let end = if line_end < 0 {
                        buf.len_bytes()
                    } else {
                        buf.line_start((line_end as usize).min(n))
                    };
                    Some(start..end.max(start))
                };
                if let Some(buf) = self.editor.buffer_of_mut(bid) {
                    buf.extmarks.clear(ns, range);
                }
            }
        }
    }

    /// Apply one [`WindowOp`] to the live editor (Phase 5) — the deferred form of
    /// the `nvim_win_*` RPC handlers, so a `vim.api.nvim_*` window call from Lua
    /// drives the same core methods. A `0` window resolves to the current one; a
    /// `0` buffer (`nvim_open_win`) to the current buffer.
    pub(crate) fn apply_window_op(&mut self, op: WindowOp) {
        let resolve_win = |s: &Self, w: u64| {
            if w == 0 {
                s.editor.current_window_id()
            } else {
                WindowId(w)
            }
        };
        match op {
            WindowOp::SetCurrent { win } => {
                let id = resolve_win(self, win);
                self.editor.set_current_window(id);
            }
            WindowOp::SetBuf { win, buf } => {
                let id = resolve_win(self, win);
                self.editor.set_window_buffer(id, BufferId(buf));
            }
            WindowOp::SetCursor { win, line, col } => {
                let id = resolve_win(self, win);
                self.editor.set_window_cursor(id, line, col);
            }
            WindowOp::SetTopline { win, top } => {
                let id = resolve_win(self, win);
                self.editor.set_window_topline(id, top);
            }
            WindowOp::SetWidth { win, width } => {
                let id = resolve_win(self, win);
                self.editor.set_window_width(id, width);
            }
            WindowOp::SetHeight { win, height } => {
                let id = resolve_win(self, win);
                self.editor.set_window_height(id, height);
            }
            WindowOp::SetOption { win, name, value } => {
                let id = resolve_win(self, win);
                // The window options nxvim honors (number / relativenumber) are
                // booleans; a numeric value for one of them is meaningless, so it
                // is ignored rather than silently coerced.
                if let OptionValue::Bool(b) = value {
                    self.editor.set_window_option_bool(id, &name, b);
                }
            }
            WindowOp::Close { win, force } => {
                let id = resolve_win(self, win);
                self.editor.close_window_by_id(id, force);
            }
            WindowOp::Open {
                buf,
                vertical,
                enter,
            } => {
                let buffer = if buf == 0 {
                    self.editor.current_buffer_id()
                } else {
                    BufferId(buf)
                };
                let prev = self.editor.current_window_id();
                self.editor.open_split_window(buffer, vertical);
                if !enter {
                    self.editor.set_current_window(prev);
                }
            }
            WindowOp::OpenFloat {
                buf,
                enter,
                relative,
                win,
                anchor,
                row,
                col,
                width,
                height,
                zindex,
                focusable,
                border,
                title,
            } => {
                let buffer = if buf == 0 {
                    self.editor.current_buffer_id()
                } else {
                    BufferId(buf)
                };
                // The prelude validated the string fields, so any unexpected value
                // here is a bug; reject loudly rather than silently mispositioning.
                let relative = match relative.as_str() {
                    "editor" => FloatRelative::Editor,
                    "cursor" => FloatRelative::Cursor,
                    "win" => {
                        let id = if win == 0 {
                            self.editor.current_window_id()
                        } else {
                            WindowId(win)
                        };
                        FloatRelative::Win(id)
                    }
                    other => {
                        self.editor
                            .echo(format!("nvim_open_win: invalid 'relative': '{other}'"));
                        return;
                    }
                };
                let anchor = match FloatAnchor::from_keyword(&anchor) {
                    Some(a) => a,
                    None => {
                        self.editor
                            .echo(format!("nvim_open_win: invalid 'anchor': '{anchor}'"));
                        return;
                    }
                };
                let border = match BorderStyle::from_keyword(&border) {
                    Some(b) => b,
                    None => {
                        self.editor
                            .echo(format!("nvim_open_win: invalid 'border': '{border}'"));
                        return;
                    }
                };
                let config = FloatConfig {
                    relative,
                    anchor,
                    row: row as isize,
                    col: col as isize,
                    width: (width as usize).max(1),
                    height: (height as usize).max(1),
                    zindex,
                    focusable,
                    border,
                    title,
                };
                self.editor.open_float_window(buffer, config, enter);
            }
            WindowOp::SetConfig {
                win,
                relative,
                parent,
                anchor,
                row,
                col,
                width,
                height,
                zindex,
                focusable,
                border,
                title,
            } => {
                let id = resolve_win(self, win);
                let mut spec = WindowConfigSpec::default();
                match relative.as_deref() {
                    None => {}
                    Some("") => spec.make_tiled = true,
                    Some("editor") => spec.relative = Some(FloatRelative::Editor),
                    Some("cursor") => spec.relative = Some(FloatRelative::Cursor),
                    Some("win") => {
                        let p = if parent == 0 {
                            self.editor.current_window_id()
                        } else {
                            WindowId(parent)
                        };
                        spec.relative = Some(FloatRelative::Win(p));
                    }
                    Some(other) => {
                        self.editor.echo(format!(
                            "nvim_win_set_config: invalid 'relative': '{other}'"
                        ));
                        return;
                    }
                }
                if let Some(a) = anchor.as_deref() {
                    match FloatAnchor::from_keyword(a) {
                        Some(v) => spec.anchor = Some(v),
                        None => {
                            self.editor
                                .echo(format!("nvim_win_set_config: invalid 'anchor': '{a}'"));
                            return;
                        }
                    }
                }
                if let Some(b) = border.as_deref() {
                    match BorderStyle::from_keyword(b) {
                        Some(v) => spec.border = Some(v),
                        None => {
                            self.editor
                                .echo(format!("nvim_win_set_config: invalid 'border': '{b}'"));
                            return;
                        }
                    }
                }
                spec.row = row.map(|v| v as isize);
                spec.col = col.map(|v| v as isize);
                spec.width = width.map(|v| v as usize);
                spec.height = height.map(|v| v as usize);
                spec.zindex = zindex;
                spec.focusable = focusable;
                spec.title = title.map(Some);
                self.editor.set_window_config(id, spec);
            }
        }
    }

    /// Apply one [`TabOp`] to the live editor (Phase 3) — the deferred form of
    /// `nvim_set_current_tabpage`, the tab analogue of [`EditHost::apply_window_op`].
    pub(crate) fn apply_tab_op(&mut self, op: TabOp) {
        match op {
            TabOp::SetCurrent { tab } => {
                let id = if tab == 0 {
                    self.editor.current_tab_id()
                } else {
                    TabId(tab)
                };
                self.editor.set_current_tabpage(id);
            }
        }
    }

    /// Refresh the Rust→Lua buffer mirror (`nx._bufs` + `nx._cur_cursor` +
    /// current window) the buffer-read API resolves against (Phase 6). Pushed
    /// before any Lua entry that can read buffer/cursor state. The per-buffer line
    /// arrays are gated on `changedtick` — only a buffer that changed since its last
    /// mirror is re-serialized — so the common cursor-moved-no-edit path only
    /// refreshes the O(1) cursor/window fields.
    pub(crate) fn push_buf_mirror(&mut self) {
        // Shift every window's jumplist to follow the line edits each buffer
        // recorded since the last pass (vim's `mark_adjust`). This runs on the same
        // universal post-mutation hook that fires `on_lines`/`on_bytes`, so it
        // covers keystroke and API edits and non-focused buffers; it drains the
        // per-buffer journals, so a call with no pending edits is a cheap no-op.
        self.editor.adjust_jumplists_for_edits();
        let mut bufs: Vec<BufMirror> = Vec::new();
        // Buffer-local option values, mirrored so `vim.bo` / `nvim_get_option_value`
        // read the core's current value (the default until set, and values set via
        // the `:set` ex path). Cheap (three scalars per buffer), so it isn't gated.
        let mut bo: Vec<BoMirror> = Vec::new();
        // The extmark snapshot for `nvim_buf_get_extmarks`: only buffers that hold
        // marks contribute, so a session with no decoration plugin pays nothing.
        let mut extmarks: Vec<(u64, Vec<ExtmarkMirror>)> = Vec::new();
        // Buffers whose text changed since the last mirror, for the `nvim_buf_attach`
        // `on_lines` callbacks: `(bufnr, changedtick, old_line_count, new_line_count)`.
        // Only buffers already known last push contribute (a buffer's first
        // appearance is its creation, not an edit), and the callbacks are fired
        // *after* every mirror is consistent (below), so a callback reading
        // `nvim_buf_get_lines` sees the new content.
        let mut changed: Vec<(u64, u64, usize, usize)> = Vec::new();
        // Buffers whose text changed since the last mirror, paired with whether they
        // were `known` last push, so the byte-delta drain below (the `nvim_buf_attach`
        // `on_bytes` channel for the `vim.treesitter` parser) can fire for the ones a
        // plugin could be attached to and discard a first-seen buffer's pre-attach
        // deltas. Carries the changedtick to stamp the `on_bytes` callback with.
        let mut fresh_ids: Vec<(BufferId, bool, u64)> = Vec::new();
        for id in self.editor.buffer_ids() {
            let tick = self
                .editor
                .buffer_of(id)
                .map(|b| b.changedtick)
                .unwrap_or(0);
            let known = self.buf_mirror_ticks.contains_key(&id);
            let fresh = self.buf_mirror_ticks.get(&id) != Some(&tick);
            let lines = if fresh {
                self.buf_mirror_ticks.insert(id, tick);
                fresh_ids.push((id, known, tick));
                Some(self.editor.lines_of(id).unwrap_or_default())
            } else {
                None
            };
            if let Some(l) = &lines {
                let new_count = l.len();
                if known {
                    let old_count = self.buf_mirror_lines.get(&id).copied().unwrap_or(new_count);
                    changed.push((id.0, tick, old_count, new_count));
                }
                self.buf_mirror_lines.insert(id, new_count);
            }
            let name = self.editor.buffer_name(id).unwrap_or_default();
            if let Some(b) = self.editor.buffer_of(id) {
                let o = b.options;
                bo.push(BoMirror {
                    bufnr: id.0,
                    tabstop: o.tabstop,
                    shiftwidth: o.shiftwidth,
                    softtabstop: o.softtabstop,
                    expandtab: o.expandtab,
                    regexsyntax: self.editor.resolve_regexsyntax(o.regexsyntax).to_string(),
                    modified: b.modified,
                    filetype: self.editor.buffer_filetype(id).unwrap_or_default(),
                    ts_highlight: self.editor.ts_highlight_enabled(id),
                });
                if !b.extmarks.is_empty() {
                    let marks = b
                        .extmarks
                        .iter_with_ns()
                        // The reserved multi-cursor namespaces (cursor heads and
                        // their visual anchors) are internal editor state, not
                        // user-visible extmarks — keep them out of the
                        // `nvim_buf_get_extmarks` mirror.
                        .filter(|(ns, _)| {
                            *ns != nxvim_core::extmark::CURSOR_NS
                                && *ns != nxvim_core::extmark::ANCHOR_NS
                        })
                        .map(|(ns, m)| {
                            let (row, col) = byte_rowcol(b, m.start);
                            let (end_row, end_col) = match m.end {
                                Some(e) => {
                                    let (r, c) = byte_rowcol(b, e);
                                    (Some(r), Some(c))
                                }
                                None => (None, None),
                            };
                            ExtmarkMirror {
                                ns,
                                id: m.id,
                                row,
                                col,
                                end_row,
                                end_col,
                                hl_group: m.hl_group.clone(),
                                priority: m.priority,
                            }
                        })
                        .collect();
                    extmarks.push((id.0, marks));
                }
            }
            bufs.push(BufMirror {
                bufnr: id.0,
                lines,
                name,
            });
        }
        // Drop tick entries for buffers that no longer exist, so the map can't grow
        // unboundedly across a long session of opening and closing buffers.
        let live: HashSet<BufferId> = self.editor.buffer_ids().into_iter().collect();
        self.buf_mirror_ticks.retain(|id, _| live.contains(id));
        self.buf_mirror_lines.retain(|id, _| live.contains(id));

        // Drain each changed buffer's Lua-treesitter byte-delta journal and project
        // it into neovim's `on_bytes` tuple for the `vim.treesitter` parser to edit
        // its trees with (fired below, once the mirrors are consistent). A `resync`
        // batch (undo/redo/`:e`) can't be replayed as deltas — signal a reload so the
        // Lua `LanguageTree` fully reparses instead. A first-seen (`!known`) buffer's
        // pre-attach deltas are discarded: no parser is attached yet, and its tree
        // (built later, on the first `get_parser`) starts from a full parse anyway.
        let mut byte_edits: Vec<BufBytesEdit> = Vec::new();
        let mut byte_reloads: Vec<u64> = Vec::new();
        for (id, known, tick) in fresh_ids {
            let Some(batch) = self.editor.take_lua_ts_edits_of(id) else {
                continue;
            };
            if !known {
                continue;
            }
            if batch.resync {
                byte_reloads.push(id.0);
            } else {
                byte_edits.extend(batch.edits.iter().map(|e| on_bytes_edit(id.0, tick, e)));
            }
        }

        let cursor = (
            (self.editor.cursor.line + 1) as u64, // 1-based row, neovim convention
            self.editor.cursor.col as u64,        // 0-based column
        );
        // The window snapshot (Phase 5): one entry per window in layout order,
        // each carrying its buffer, cursor (1-based row / 0-based col), and text
        // dimensions, so the `nvim_win_*` getters read live state from Lua.
        let wins: Vec<WindowMirror> = self
            .editor
            .window_ids()
            .into_iter()
            .map(|id| {
                let buffer = self.editor.window_buffer(id).map(|b| b.0).unwrap_or(0);
                let (line, col) = self.editor.window_cursor(id).unwrap_or((0, 0));
                let (cw, ch) = self.editor.window_content_size(id).unwrap_or((0, 0));
                let opts = self.editor.window_options(id).unwrap_or_default();
                let (top, leftcol) = self.editor.window_scroll(id).unwrap_or((0, 0));
                let (jumps, jump_idx) = self.editor.window_jumplist(id).unwrap_or_default();
                WindowMirror {
                    id: id.0,
                    buffer,
                    row: (line + 1) as u64,
                    col: col as u64,
                    // The content size `nvim_win_get_width`/`get_height` report:
                    // the gutter is included, a bordered float's border and a
                    // window's status row are not (matches what's drawn).
                    width: cw as u64,
                    height: ch as u64,
                    number: opts.number,
                    relativenumber: opts.relativenumber,
                    // `winsaveview()` reports `topline` 1-based; `top` is 0-based.
                    topline: (top + 1) as u64,
                    leftcol: leftcol as u64,
                    float: self.editor.window_float_config(id).map(float_mirror),
                    jumps: jumps
                        .into_iter()
                        .map(|(bufnr, line, col)| JumpMirror {
                            bufnr,
                            lnum: (line + 1) as u64, // 1-based row, neovim convention
                            col: col as u64,         // 0-based byte column
                            coladd: 0,               // nxvim has no `virtualedit`
                        })
                        .collect(),
                    jump_idx: jump_idx as u64,
                }
            })
            .collect();
        let cur_win_id = self.editor.current_window_id();
        let cur_win = cur_win_id.0;
        let next_win = self.editor.next_window_id().0;
        // The focused window cursor's whole-screen position (1-based), for
        // `vim.fn.screenrow`/`screencol`: the window's rect origin, past its number
        // gutter, plus the cursor offset within the scrolled viewport. INCOMPLETE:
        // the column uses the cursor's byte offset, not its display column (tabs /
        // wide chars aren't expanded), and the row ignores a `'showtabline'` offset
        // — close enough for a popup plugin's cursor-overlap check, off by a tab's width
        // / one row in those cases.
        if let (Some((wx, wy, _, _)), Some((top, leftcol)), Some(textoff)) = (
            self.editor.window_rect(cur_win_id),
            self.editor.window_scroll(cur_win_id),
            self.editor.window_textoff(cur_win_id),
        ) {
            let (cl, cc) = (self.editor.cursor.line, self.editor.cursor.col);
            let srow = wy + cl.saturating_sub(top) + 1;
            let scol = wx + textoff + cc.saturating_sub(leftcol) + 1;
            let _ = self.lua.set_screen_cursor(srow as u64, scol as u64);
        }
        let _ = self.lua.set_buf_mirror(
            &bufs,
            cursor,
            cur_win,
            &wins,
            next_win,
            self.editor.mode.short_code(),
            self.editor.cmdline_type(),
        );
        let _ = self.lua.set_bo_mirror(&bo);
        let _ = self.lua.set_extmark_mirror(&extmarks);
        // The highlight registry, mirrored so `nvim_get_hl` reads live group
        // definitions from Lua. Gated on the registry's generation — a colorscheme
        // populates hundreds of groups once and rarely changes them, so re-pushing
        // the whole table every chunk would be wasteful; only a real change (a
        // `:hi` / `nvim_set_hl` / `:colorscheme`) re-serializes it.
        let hl_gen = self.editor.highlights.generation();
        if self.hl_mirror_gen != Some(hl_gen) {
            self.hl_mirror_gen = Some(hl_gen);
            let mirror = |ns: u32, name: &str, def: &nxvim_core::highlight::HlDef| HlDefMirror {
                ns,
                name: name.to_string(),
                fg: def.fg.map(|c| c.to_u32()),
                bg: def.bg.map(|c| c.to_u32()),
                sp: def.sp.map(|c| c.to_u32()),
                bold: def.bold,
                italic: def.italic,
                underline: def.underline,
                undercurl: def.undercurl,
                strikethrough: def.strikethrough,
                reverse: def.reverse,
                link: def.link.clone(),
            };
            let defs: Vec<HlDefMirror> = self
                .editor
                .highlights
                .iter()
                .map(|(name, def)| mirror(0, name, def))
                .collect();
            let _ = self.lua.set_hl_mirror(&defs);
            // Non-zero namespaces ride a separate mirror (`nx._hl_defs_ns`) so
            // `nvim_get_hl(ns, …)` reads them without touching the global table.
            let ns_defs: Vec<HlDefMirror> = self
                .editor
                .highlights
                .iter_namespaces()
                .map(|(ns, name, def)| mirror(ns, name, def))
                .collect();
            let _ = self.lua.set_hl_mirror_ns(&ns_defs);
        }
        // The tab snapshot (Phase 3): one entry per tab page in tabline order, each
        // carrying its window ids and focused window, so `nvim_tabpage_*` reads from
        // Lua resolve against live state.
        let tabs: Vec<TabMirror> = self
            .editor
            .tab_ids()
            .into_iter()
            .map(|id| TabMirror {
                id: id.0,
                windows: self
                    .editor
                    .tab_window_ids(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| w.0)
                    .collect(),
                buffers: self
                    .editor
                    .tab_window_buffers(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|b| b.0)
                    .collect(),
                current_window: self.editor.tab_current_window(id).map(|w| w.0).unwrap_or(0),
            })
            .collect();
        let _ = self
            .lua
            .set_tab_mirror(&tabs, self.editor.current_tab_id().0);
        // Global options, mirrored so `vim.o` reads the core's current value (the
        // default until set, and values set via the `:set` ex path). Cheap (the
        // five search flags, showtabline/laststatus, statusline/tabline/guifont,
        // and the screen columns/lines), so it isn't gated.
        let go = self.editor.global_options();
        let (columns, lines) = self.editor.screen_size();
        let _ = self.lua.set_go_mirror(&GoMirror {
            ignorecase: go.ignorecase,
            smartcase: go.smartcase,
            wrapscan: go.wrapscan,
            hlsearch: go.hlsearch,
            incsearch: go.incsearch,
            showtabline: go.showtabline,
            laststatus: go.laststatus,
            statusline: go.statusline.clone(),
            tabline: go.tabline.clone(),
            guifont: go.guifont.clone(),
            regexsyntax: go.regexsyntax.clone(),
            autoread: go.autoread,
            scrollanim: go.scrollanim,
            scrollanimduration: go.scrollanimduration as u64,
            columns: columns as u64,
            lines: lines as u64,
        });
        // The register file, mirrored so `vim.fn.getreg` / `getregtype` read the
        // core's current registers (stored cells + the read-only specials). Small
        // (a handful of short strings), so it isn't gated on a dirty flag.
        let regs = self.editor.register_mirror();
        let _ = self.lua.set_reg_mirror(&regs);
        // The `vim.v.*` predefined variables sourced from the pending-command
        // state (`v:count` / `v:count1` / `v:register` / `v:operator`), so a
        // keymap RHS / `<expr>` reading them reflects the count/register typed
        // before it fired. Cheap (four scalars), so it isn't gated.
        let _ = self.lua.set_v_mirror(
            self.editor.pending_count() as u64,
            self.editor.pending_count1() as u64,
            &self.editor.pending_register().to_string(),
            &self
                .editor
                .pending_operator()
                .map(String::from)
                .unwrap_or_default(),
        );
        self.push_undotree_mirror();
        // Now that every mirror is consistent, fire the `nvim_buf_attach` callbacks.
        // `on_bytes` (and `on_reload`) go first — they edit the `vim.treesitter`
        // parser's trees so the next `:parse()` reparses incrementally — then
        // `on_lines`, whose callbacks read the refreshed buffer via
        // `nvim_buf_get_lines` and schedule follow-up work (a fuzzy-finder plugin
        // re-runs its finder), drained by the enclosing `run_pending` fixpoint.
        if !byte_reloads.is_empty() {
            let _ = self.lua.fire_buf_reloads(&byte_reloads);
        }
        if !byte_edits.is_empty() {
            let _ = self.lua.fire_buf_bytes(&byte_edits);
        }
        if !changed.is_empty() {
            let _ = self.lua.fire_buf_changes(&changed);
        }
    }

    /// Refresh the `nx._undotree` mirror that `vim.fn.undotree(bufnr)` reads.
    /// Only buffers whose undo fingerprint changed since the last push are
    /// re-projected (the tree walk is O(history), so this keeps the hot
    /// buffer-mirror path cheap when nothing edited).
    pub(crate) fn push_undotree_mirror(&mut self) {
        let ids = self.editor.buffer_ids();
        let live: Vec<u64> = ids.iter().map(|id| id.0).collect();
        let mut updates: Vec<(u64, Value)> = Vec::new();
        for id in ids {
            let version = self.editor.undo_version(id);
            if self.undo_mirror_versions.get(&id) == Some(&version) {
                continue;
            }
            updates.push((id.0, undotree_value(&self.editor.undotree_of(id))));
            self.undo_mirror_versions.insert(id, version);
        }
        let pruned = self.undo_mirror_versions.len() != live.len();
        self.undo_mirror_versions
            .retain(|id, _| live.contains(&id.0));
        if !updates.is_empty() || pruned {
            let _ = self.lua.set_undotree_mirror(&updates, &live);
        }
    }

    /// Route one [`LoopOp`]: enqueue a `Schedule` for the `run_pending` drain, or
    /// forward a timer / process op to the event-loop actor (a fire-and-forget
    /// [`LoopCommand`], never awaited).
    pub(crate) fn apply_loop_op(&mut self, op: LoopOp) {
        match op {
            // `vim.schedule` needs no event loop — the id queues for the trailing
            // `run_pending` drain — so it works in every build.
            LoopOp::Schedule { id } => self.scheduled.push_back(id),
            // Timers / processes / fs-watches ride the tokio event loop. Native only
            // for now; the Worker-side timer wheel is slice 5d.
            #[cfg(feature = "native")]
            LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            } => self.fx.loop_command(LoopCommand::TimerStart {
                id,
                delay: std::time::Duration::from_millis(delay_ms),
                repeat: std::time::Duration::from_millis(repeat_ms),
            }),
            #[cfg(feature = "native")]
            LoopOp::TimerStop { id } => self.fx.loop_command(LoopCommand::TimerStop { id }),
            #[cfg(feature = "native")]
            LoopOp::Spawn {
                id,
                cmd,
                cwd,
                env,
                stdin,
            } => self.fx.loop_command(LoopCommand::Spawn {
                id,
                argv: cmd,
                cwd,
                env,
                stdin,
            }),
            #[cfg(feature = "native")]
            LoopOp::Kill { id } => self.fx.loop_command(LoopCommand::Kill { id }),
            // The browser build has no tokio event loop; timers ride the Worker-side
            // wheel instead (slice 5d) — `vim.defer_fn` / `nx.timer` arm and fire there.
            #[cfg(not(feature = "native"))]
            LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            } => self.arm_wasm_timer(id, delay_ms, repeat_ms),
            #[cfg(not(feature = "native"))]
            LoopOp::TimerStop { id } => self.stop_wasm_timer(id),
            // Processes / fs-watch still have no serverless analogue (those need the
            // Phase 6 daemon): surface the unsupported request loudly, never silently.
            #[cfg(not(feature = "native"))]
            LoopOp::Spawn { .. } | LoopOp::Kill { .. } => self.editor.echo(
                "E: jobs/processes (vim.system / jobstart / vim.uv.spawn) are not \
                 available in the browser build yet",
            ),
        }
    }

    /// Handle one completion from the event-loop actor (a timer fired, a child
    /// reported its pid, or a child exited) by running its Lua callback on the
    /// server thread, then draining the effects it queued. The caller's
    /// `settle_events` drives the rest to convergence and repaints once per burst.
    /// Native only — it arrives on the run loop's `loop_events` arm, which the wasm
    /// build doesn't have (its inbound side is slice 5c).
    #[cfg(feature = "native")]
    pub(crate) fn on_loop_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::Timer { id, keep } => {
                if let Err(e) = self.lua.run_callback(id, keep, CallbackArgs::None) {
                    self.editor
                        .echo(format!("E5108: Error in timer callback: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::ProcessSpawned { id, pid } => {
                // Record the real pid so the `vim.system` handle's `.pid` resolves
                // it (it can't be known synchronously on a single-threaded runtime).
                if let Err(e) = self.lua.set_process_pid(id, pid) {
                    self.editor
                        .echo(format!("E5108: Error recording process pid: {e}"));
                }
            }
            LoopEvent::ProcessExit {
                id,
                code,
                stdout,
                stderr,
            } => {
                let args = CallbackArgs::Process {
                    code,
                    stdout,
                    stderr,
                };
                if let Err(e) = self.lua.run_callback(id, false, args) {
                    self.editor
                        .echo(format!("E5108: Error in vim.system on_exit: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::FsEvent { id, error, .. } => {
                // An internal per-buffer file watch's auto-trigger (the only
                // `fs_event` producer — the Lua `vim.uv.fs_event` surface is gone).
                // The file under buffer `id - BASE` changed, so enqueue its reconcile
                // — the trailing `settle_events` → `run_pending` fires the
                // `FileChangedShell` round-trip (autoreload / a handler's
                // `v:fcs_choice` / W11 / W12 / E211) and re-arms the watch after any
                // reload (a reload re-stamps the disk snapshot, so the watch key
                // changed). `error` (the watch failed to arm) just drops the watch
                // state and stops — a *later* tick (a lifecycle event or a reconcile)
                // re-arms it. It must NOT re-arm here: re-arming the same failing key
                // in place would fail again and re-enter this arm, spinning forever
                // (an absent path can't be watched by kqueue/inotify). `sync_buffer_watches`
                // already declines to arm a not-yet-written new-file buffer (no disk
                // snapshot), so the common case never reaches here; this is the backstop
                // for a transient arm failure (e.g. the file vanished between the key
                // snapshot and the arm).
                let buf = BufferId(id - crate::INTERNAL_WATCH_BASE);
                if error.is_some() {
                    self.buf_watches.remove(&buf);
                } else {
                    self.editor.checktime_buffer(buf);
                }
            }
        }
    }

    /// The settle contract for an off-tick event arm: drive every queued effect to
    /// convergence (`run_pending`, which also drains `self.scheduled`) and repaint
    /// once. `dirty` forces a repaint even when no Lua callback ran (e.g. an LSP
    /// event that only updated cached state); a callback that queued work always
    /// repaints. Factored out so the syntax/LSP/loop arms share one tail and no
    /// off-tick callback's deferred `vim.cmd` is left undriven.
    pub(crate) fn settle_events(&mut self, dirty: bool) {
        let had_scheduled = !self.scheduled.is_empty();
        self.run_pending();
        // An off-tick callback (a timer, a scheduled fn) may have queued
        // `nvim_feedkeys` — e.g. a plugin's deferred re-feed; process
        // that typeahead now, the off-tick analogue of `input`'s trailing drain.
        let had_feed = !self.feed_buffer.is_empty();
        self.drain_feedkeys();
        if dirty || had_scheduled || had_feed {
            self.redraw();
        }
    }

    /// Drive queued work to convergence: run the `:lua` chunks the editor
    /// queued, resolve every ex-command the core deferred (a Lua user command,
    /// else the unknown-command error), and repeat until nothing new is queued.
    /// Both queues feed each other — a user command can `vim.cmd(...)`, a `:lua`
    /// can define a command — so a single fixpoint loop covers them.
    pub(crate) fn run_pending(&mut self) {
        // Cap on fixpoint rounds before we conclude the queued work is
        // self-perpetuating — a command or `on_select` callback that re-queues
        // itself every round (e.g. a user command whose body re-runs the same
        // command). Without this the single-threaded server spins forever and
        // stops servicing input. Generous enough that any legitimate finite
        // chain converges first; mirrors neovim's `maxfuncdepth` recursion guard.
        const MAX_ROUNDS: usize = 100;
        let mut rounds = 0;
        // Refresh the buffer mirror before draining: everything that flows through
        // `run_pending` (user commands, scheduled / select callbacks, queued `:lua`)
        // can read buffer/cursor state. The Lua API exposes no buffer-text write, so
        // the mirror can't go stale mid-batch from Lua — once-at-entry is enough.
        self.push_buf_mirror();
        // Whether any `:checktime` / watch reconcile reloaded a buffer this drain —
        // a reload re-stamps the disk snapshot (a new inode after an atomic replace),
        // so the per-buffer watch must re-arm against the new key once we settle.
        // Native only — the serverless browser build has no on-disk file to reconcile.
        #[cfg(feature = "native")]
        let mut reconciled = false;
        loop {
            // File-change reconciles core deferred (`:checktime`, or the per-buffer
            // file watch): fire the `FileChangedShell` round-trip and apply the
            // choice. Inside the fixpoint so a handler's queued `vim.cmd`/`:lua`
            // drains in the same convergence (and a handler that re-runs `:checktime`
            // keeps draining via the break check below).
            #[cfg(feature = "native")]
            for buf in self.editor.take_pending_checktime() {
                reconciled = true;
                self.reconcile_file_change(buf);
            }
            for chunk in std::mem::take(&mut self.editor.lua_queue) {
                if let Err(e) = self.lua.exec(&chunk) {
                    self.editor.echo(format!("E5108: Error executing lua: {e}"));
                }
                self.apply_lua_effects();
            }
            for cmd in std::mem::take(&mut self.editor.deferred_commands) {
                self.resolve_command(&cmd);
            }
            // `<CR>` selections on a select-enabled panel: notify RPC clients and
            // fire the Lua `on_select` callback. The callback may itself queue
            // commands / lua / panel ops, so this is inside the fixpoint loop.
            for (index, line) in std::mem::take(&mut self.editor.panel_selects) {
                // The `:LspCodeAction` list (Phase 6) is a select-enabled panel:
                // a `<CR>` on row `index` applies that action's edit, keyed to the
                // currently-open code-action panel by title so a select on some
                // *other* select panel can't misroute here. (Native only — the
                // browser build has no code actions.)
                #[cfg(feature = "native")]
                if self.editor.panel_title() == Some(CODE_ACTION_PANEL_TITLE) {
                    self.apply_code_action(index);
                    continue;
                }
                // Navigable LSP location lists (diagnostics, references) jump in
                // the core itself when their target line is selected, so they
                // never reach here — only scripted/RPC select panels do.
                self.fx.notify(
                    "nxvim_panel_select",
                    vec![Value::Map(vec![
                        (Value::from("index"), Value::from(index as u64 + 1)),
                        (Value::from("line"), Value::from(line.as_str())),
                    ])],
                );
                if let Err(e) = self.lua.run_panel_select(index, &line) {
                    self.editor
                        .echo(format!("E5108: Error in panel on_select: {e}"));
                }
                self.apply_lua_effects();
            }
            // `vim.ui.input` results (Phase 8): a submitted (`Some`) or cancelled
            // (`None`) prompt fires the waiting callback off the same tick. The
            // callback may itself open another prompt / queue lua, so this is
            // inside the fixpoint. The pending id is taken (one prompt at a time).
            for result in std::mem::take(&mut self.editor.prompt_results) {
                if let Some(id) = self.pending_ui_input.take() {
                    if let Err(e) = self.lua.run_ui_input(id, result) {
                        self.editor
                            .echo(format!("E5108: Error in vim.ui.input callback: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // Scheduled callbacks (`vim.schedule`) run after the work that queued
            // them converges, but still within this fixpoint — a scheduled fn may
            // itself `vim.schedule` / `vim.cmd`, which re-enters the loop. One
            // throwing callback is isolated (echoed as E5108) and never aborts the
            // drain or stops a later scheduled callback from running.
            for id in std::mem::take(&mut self.scheduled) {
                if let Err(e) = self.lua.run_callback(id, false, CallbackArgs::None) {
                    self.editor
                        .echo(format!("E5108: Error in scheduled callback: {e}"));
                }
                self.apply_lua_effects();
            }
            if self.editor.lua_queue.is_empty()
                && self.editor.deferred_commands.is_empty()
                && self.editor.panel_selects.is_empty()
                && self.editor.prompt_results.is_empty()
                && self.scheduled.is_empty()
                && !self.editor.has_pending_checktime()
            {
                break;
            }
            rounds += 1;
            if rounds >= MAX_ROUNDS {
                // Drop the still-growing work and report it, rather than loop
                // forever. The editor stays responsive to the next message.
                self.editor.lua_queue.clear();
                self.editor.deferred_commands.clear();
                self.editor.panel_selects.clear();
                self.editor.prompt_results.clear();
                self.editor.take_pending_checktime();
                self.scheduled.clear();
                self.editor
                    .echo("E132: command recursion limit exceeded".to_string());
                break;
            }
        }
        // The drained work may have changed the buffer/window topology (a queued
        // `:lua` window op, a `vim.cmd('split')`, a buffer switch). Diff once more
        // so the resulting `WinNew`/`WinEnter`/`BufEnter`/… autocmds fire — the
        // batch boundary, after everything has settled. Idempotent: a no-op when
        // nothing changed since the last per-key diff (the common case).
        self.emit_lifecycle_events();
        // A reconcile that reloaded a buffer changed its `(path, disk-stat)` watch key
        // (a fresh inode after an atomic replace); re-arm the per-buffer watch so it
        // follows the file. Idempotent — `sync_buffer_watches` no-ops when keys match.
        #[cfg(feature = "native")]
        if reconciled {
            self.sync_buffer_watches();
        }
        // Route any buffer I/O core deferred this convergence onto the daemon wire
        // (off-tick mode): writes (`:w`) and opens (`:edit`). No-ops when off-tick mode
        // is off or none ran.
        self.drain_pending_saves();
        self.drain_pending_quit_all();
        self.drain_pending_opens();
        // Explicit `:wshada` / `:rshada` raised this convergence: flush / re-merge the
        // store. After the opens/saves drain so a `:rshada` sees the settled session.
        // Native only — the shada store (redb) is gated off the wasm build (slice 5a).
        #[cfg(feature = "native")]
        self.drain_pending_shada();
    }
}

/// Translate a Lua-side `nvim_set_hl` definition into the core registry's
/// `HlDef`, parsing the color strings (`#rrggbb` / named / `NONE`) here at the
/// boundary so `nxvim-lua` need not know about the color type.
fn hl_def(hl: &HlSet) -> HlDef {
    let color = |c: &Option<String>| c.as_deref().and_then(parse_color);
    HlDef {
        fg: color(&hl.fg),
        bg: color(&hl.bg),
        sp: color(&hl.sp),
        bold: hl.bold,
        italic: hl.italic,
        underline: hl.underline,
        undercurl: hl.undercurl,
        strikethrough: hl.strikethrough,
        reverse: hl.reverse,
        link: hl.link.clone(),
    }
}
