//! Server-side LSP integration: the `syntax.rs` analogue for language servers.
//!
//! Where `nxvim-lsp` owns the client machinery (spawning/supervising servers and
//! the JSON-RPC bridge — the `SyntaxClient` analogue), this module owns the
//! *editor* half: the built-in filetype→server config table, per-buffer
//! document-sync bookkeeping ([`LspDocState`], keyed by [`BufferId`] like
//! `SyntaxState`), byte↔LSP position conversion via `nxvim-core`'s pure unicode
//! helpers, and the handling of [`LspEvent`]s. Document sync reuses the buffer
//! edit journal (`take_edits`/`changedtick`) the syntax sync already drives
//! (Decision 5); the only added core signal is `Buffer::save_tick`, a monotonic
//! write counter so `didSave` fires off a real save hook rather than a heuristic.
//!
//! All [`Server`] methods here run on the single editor thread and only ever
//! hand the manager fire-and-forget [`LspNotify`]s, so a slow or hung server can
//! never stall keystroke→buffer→redraw.

use std::path::{Path, PathBuf};

use nxvim_core::unicode;
use nxvim_core::{BufferEdit, BufferId};
use nxvim_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, TextDocumentContentChangeEvent,
    TextDocumentSyncKind, Url,
};
use nxvim_lsp::{LspEvent, LspNotify, PositionEncoding, ServerKey, ServerSpawn};
use rmpv::Value;

use crate::{filetype_of, Server, StyleTable};

/// One entry in the `:LspDiagnostics` location list: the target file (the
/// buffer's path, `None` for an unnamed buffer), 0-based line, and 0-based
/// **byte** column the `<CR>` jump lands on (the LSP character already converted
/// through the negotiated encoding). Indexed in lockstep with the panel's lines.
pub(crate) type DiagLocation = (Option<PathBuf>, usize, usize);

/// Per-buffer LSP document-sync bookkeeping, mirroring `SyntaxState`. One per
/// open buffer that mapped to a configured server, keyed by [`BufferId`] in
/// [`Server::lsp_states`].
#[derive(Default)]
pub(crate) struct LspDocState {
    /// Which server owns this buffer (`None` for an unsupported filetype).
    server: Option<ServerKey>,
    /// The document URI, kept so `didClose` can be sent after the buffer is gone.
    uri: Option<Url>,
    /// Has `didOpen` been sent for the current server instance?
    opened: bool,
    /// LSP document version (monotonic, bumped per `didChange`).
    version: i32,
    /// `changedtick` of the last sync we sent (drives `didChange`).
    last_tick: u64,
    /// `save_tick` of the last sync, mirrored to fire `didSave` exactly when the
    /// buffer is written (`save_tick` bumps only on a successful `:w`).
    last_save_tick: u64,
    /// Latest `publishDiagnostics` for this buffer, projected into the redraw
    /// (`diagnostics_for`) and the under-cursor message line.
    diagnostics: Vec<Diagnostic>,
}

/// The negotiated runtime state of one server, learned from its `initialize`
/// reply: the position encoding and document-sync kind every buffer it owns uses.
pub(crate) struct ServerRuntime {
    encoding: PositionEncoding,
    sync_kind: TextDocumentSyncKind,
}

impl Server {
    /// Drive LSP document sync for the *current* buffer this frame: ensure its
    /// server is started, then send `didOpen`/`didChange`/`didSave` as the buffer
    /// state requires. Called from `redraw()` alongside `sync_syntax`. Never
    /// blocks: every send is a fire-and-forget [`LspNotify`].
    pub(crate) fn sync_lsp(&mut self) {
        self.reap_closed_lsp_buffers();

        let buffer = self.editor.current_buffer_id();
        let Some(path) = self.editor.buffer().path.clone() else {
            self.clear_lsp_server(buffer);
            return;
        };
        let Some(language) = filetype_of(Some(&path)) else {
            self.clear_lsp_server(buffer);
            return;
        };
        let key = ServerKey {
            language,
            root: workspace_root(&path),
        };
        // Ensure the server exactly once per key (the `ensure_started` analogue):
        // resolving the command scans `PATH`, and `ensure_server` is a channel
        // send — neither should run on every redraw. A filetype with no
        // configured/installed server clears the buffer's server marker and stops.
        if !self.lsp_ensured.contains(&key) {
            let Some(spawn) = lsp_spawn_for(language) else {
                self.clear_lsp_server(buffer);
                return;
            };
            self.lsp.ensure_server(key.clone(), spawn);
            self.lsp_ensured.insert(key.clone());
        }
        let Some(uri) = path_to_uri(&path) else {
            return;
        };

        // The encoding/sync kind aren't known until the server's `initialize`
        // reply lands (the `Initialized` event). Until then, just remember the
        // intended server so the buffer opens as soon as it's ready.
        let Some(&ServerRuntime {
            encoding,
            sync_kind,
        }) = self.lsp_servers.get(&key)
        else {
            let state = self.lsp_states.entry(buffer).or_default();
            state.server = Some(key);
            state.uri = Some(uri);
            return;
        };

        let cur_tick = self.editor.buffer().changedtick;
        let cur_save_tick = self.editor.buffer().save_tick;

        let mut state = self.lsp_states.remove(&buffer).unwrap_or_default();
        state.server = Some(key.clone());
        state.uri = Some(uri.clone());

        // A text change since the last sync (only meaningful once opened).
        let tick_changed = state.opened && cur_tick != state.last_tick;

        if !state.opened {
            // First open (or re-open after a respawn): full text supersedes any
            // journaled deltas, so drop them.
            let _ = self.editor.buffer_mut().take_edits();
            let text = self.editor.buffer().text.to_string();
            state.version = 1;
            self.lsp.notify(
                key.clone(),
                LspNotify::DidOpen {
                    uri: uri.clone(),
                    language_id: language.to_string(),
                    version: state.version,
                    text,
                },
            );
            state.opened = true;
            state.last_tick = cur_tick;
            // The freshly-opened content is the on-disk state, so don't fire a
            // spurious `didSave` for saves that predate the open.
            state.last_save_tick = cur_save_tick;
        } else if tick_changed && sync_kind != TextDocumentSyncKind::NONE {
            let batch = self.editor.buffer_mut().take_edits();
            state.version += 1;
            // Full sync (server's choice, or a whole-rope replacement where deltas
            // are meaningless) sends the entire text; otherwise incremental deltas.
            let changes = if batch.resync || sync_kind == TextDocumentSyncKind::FULL {
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: self.editor.buffer().text.to_string(),
                }]
            } else {
                self.incremental_changes(&batch.edits, encoding)
            };
            self.lsp.notify(
                key.clone(),
                LspNotify::DidChange {
                    uri: uri.clone(),
                    version: state.version,
                    changes,
                },
            );
            state.last_tick = cur_tick;
        }

        // Save: the buffer's write counter advanced since the last sync, so a `:w`
        // landed bytes on disk (a real hook, not a `modified`-flag heuristic).
        if state.opened && cur_save_tick != state.last_save_tick {
            self.lsp.notify(key, LspNotify::DidSave { uri, text: None });
            state.last_save_tick = cur_save_tick;
        }

        self.lsp_states.insert(buffer, state);
    }

    /// Mark a buffer as no longer served (its filetype lost a server). The doc
    /// state is kept so a later re-detection re-opens cleanly.
    fn clear_lsp_server(&mut self, buffer: BufferId) {
        if let Some(state) = self.lsp_states.get_mut(&buffer) {
            state.server = None;
        }
    }

    /// Send `didClose` for, and forget the state of, every buffer the editor has
    /// since deleted (`:bdelete`) — the LSP analogue of `reap_closed_buffers`.
    fn reap_closed_lsp_buffers(&mut self) {
        let live = self.editor.buffer_ids();
        let dead: Vec<BufferId> = self
            .lsp_states
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in dead {
            if let Some(state) = self.lsp_states.remove(&id) {
                if let (true, Some(key), Some(uri)) = (state.opened, state.server, state.uri) {
                    self.lsp.notify(key, LspNotify::DidClose { uri });
                }
            }
        }
    }

    /// Handle one [`LspEvent`] drained from the manager. Phase 1 acts on
    /// `Initialized` (record encoding/caps, schedule re-open), caches
    /// `Diagnostics`, logs server messages, and tolerates exits (the manager
    /// respawns or gives up; buffers stay editable).
    pub(crate) fn on_lsp_event(&mut self, event: LspEvent) {
        match event {
            LspEvent::Initialized {
                key,
                caps,
                encoding,
            } => {
                self.lsp_servers.insert(
                    key.clone(),
                    ServerRuntime {
                        encoding,
                        sync_kind: caps.sync_kind,
                    },
                );
                // A fresh (or respawned) server holds no documents: re-open every
                // buffer bound to it on the next sync. This doubles as the restart
                // handler, like `SyntaxEvent::Restarted`.
                for state in self.lsp_states.values_mut() {
                    if state.server.as_ref() == Some(&key) {
                        state.opened = false;
                        state.version = 0;
                    }
                }
                self.lsp_dirty = true;
            }
            LspEvent::Diagnostics {
                uri, diagnostics, ..
            } => {
                // Cache the latest publish for the matching buffer; the redraw
                // projects whichever buffer is current (route by `uri`, dropping
                // a publish for a buffer closed while it was in flight, as
                // `store_spans` drops unknown-buffer syntax replies). Mark dirty
                // so the coalesced repaint paints the new squiggles.
                if let Some(state) = self
                    .lsp_states
                    .values_mut()
                    .find(|s| s.uri.as_ref() == Some(&uri))
                {
                    state.diagnostics = diagnostics;
                    self.lsp_dirty = true;
                }
            }
            LspEvent::ServerExited { .. } => {
                // The manager respawns per its breaker (or gives up cleanly). The
                // editor stays fully responsive throughout; nothing to surface yet.
            }
            LspEvent::Log { message, .. } => {
                // Record to `:messages` without disturbing the message line.
                self.editor.messages.push(message);
            }
        }
    }

    /// Build the `:LspInfo` report: the current buffer's server/encoding/sync/
    /// version/diagnostics, then every running server and every attached buffer.
    /// Phase-1 observability while there is no on-screen LSP feature yet.
    pub(crate) fn lsp_info_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let current = self.editor.current_buffer_id();

        lines.push("Current buffer".to_string());
        match self.lsp_states.get(&current).filter(|s| s.server.is_some()) {
            Some(state) => {
                let key = state.server.as_ref().unwrap();
                let runtime = self.lsp_servers.get(key);
                lines.push(format!(
                    "  server:      {} ({})",
                    key.language,
                    key.root.display()
                ));
                lines.push(format!(
                    "  status:      {}",
                    if !self.lsp_ensured.contains(key) {
                        "not started"
                    } else if runtime.is_none() {
                        "starting (awaiting initialize)"
                    } else if state.opened {
                        "attached"
                    } else {
                        "initialized (didOpen pending)"
                    }
                ));
                if let Some(runtime) = runtime {
                    lines.push(format!(
                        "  encoding:    {}    sync: {}",
                        encoding_label(runtime.encoding),
                        sync_label(runtime.sync_kind),
                    ));
                }
                lines.push(format!("  version:     {}", state.version));
                lines.push(format!("  diagnostics: {}", state.diagnostics.len()));
            }
            None => lines.push("  (no language server for this buffer)".to_string()),
        }

        lines.push(String::new());
        lines.push("Running servers".to_string());
        if self.lsp_servers.is_empty() {
            lines.push("  (none)".to_string());
        } else {
            let mut servers: Vec<_> = self.lsp_servers.iter().collect();
            servers.sort_by_key(|(k, _)| (k.language, k.root.clone()));
            for (key, runtime) in servers {
                let attached = self
                    .lsp_states
                    .values()
                    .filter(|s| s.opened && s.server.as_ref() == Some(key))
                    .count();
                lines.push(format!(
                    "  {} @ {} — {}, {}, {attached} buffer(s)",
                    key.language,
                    key.root.display(),
                    encoding_label(runtime.encoding),
                    sync_label(runtime.sync_kind),
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!(
            "Log: {}",
            std::env::var("NXVIM_LSP_LOG_FILE").unwrap_or_else(|_| {
                "$XDG_STATE_HOME/nxvim/lsp.log (or ~/.local/state/nxvim/lsp.log)".to_string()
            })
        ));
        lines
    }

    /// The current buffer's cached diagnostics together with its server's
    /// negotiated position encoding, or `None` when the buffer has no attached
    /// server (so callers project nothing). Both borrows are released before any
    /// `&mut self` use.
    fn current_diagnostics(&self) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
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
        let Some((diags, encoding)) = self.current_diagnostics() else {
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
    fn diag_row_span(
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
    /// a parallel [`DiagLocation`] list the `<CR>` jump indexes. `None` when the
    /// buffer has no diagnostics.
    pub(crate) fn diagnostics_location_list(&self) -> Option<(Vec<String>, Vec<DiagLocation>)> {
        let (diags, encoding) = self.current_diagnostics()?;
        if diags.is_empty() {
            return None;
        }
        let path = self.editor.buffer().path.clone();
        let mut items: Vec<&Diagnostic> = diags.iter().collect();
        items.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        let mut lines = Vec::with_capacity(items.len());
        let mut locations = Vec::with_capacity(items.len());
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
            locations.push((path.clone(), row, byte_col(encoding, &line, character)));
        }
        Some((lines, locations))
    }

    /// Convert a batch of journaled byte-delta edits into LSP incremental content
    /// changes, each replacing the edit's old `(start..old_end)` range with its
    /// inserted text, in the server's negotiated position encoding.
    fn incremental_changes(
        &self,
        edits: &[BufferEdit],
        encoding: PositionEncoding,
    ) -> Vec<TextDocumentContentChangeEvent> {
        edits
            .iter()
            .map(|e| TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: self.lsp_position(encoding, e.start_point.0, e.start_point.1),
                    end: self.lsp_position(encoding, e.old_end_point.0, e.old_end_point.1),
                }),
                range_length: None,
                text: e.text.clone(),
            })
            .collect()
    }

    /// A buffer `(row, byte-column)` point as an LSP [`Position`], converting the
    /// byte column to the server's encoding (Decision 4): UTF-8 is the identity
    /// (an LSP UTF-8 character *is* a byte offset), UTF-16/UTF-32 need column
    /// math over the line text.
    fn lsp_position(&self, encoding: PositionEncoding, row: usize, byte_col: usize) -> Position {
        let character = match encoding {
            PositionEncoding::Utf8 => byte_col,
            PositionEncoding::Utf16 => {
                let line = self.editor.buffer().line(row);
                unicode::byte_to_utf16(&line, byte_col)
            }
            PositionEncoding::Utf32 => {
                let line = self.editor.buffer().line(row);
                line[..byte_col.min(line.len())].chars().count()
            }
        };
        Position {
            line: row as u32,
            character: character as u32,
        }
    }
}

/// Human label for a negotiated position encoding (matches the LSP wire names).
fn encoding_label(encoding: PositionEncoding) -> &'static str {
    match encoding {
        PositionEncoding::Utf8 => "utf-8",
        PositionEncoding::Utf16 => "utf-16",
        PositionEncoding::Utf32 => "utf-32",
    }
}

/// Byte offset of LSP `character` on `line`, the inverse of [`Server::lsp_position`]
/// (Decision 4): UTF-8 is the identity (a character *is* a byte offset, clamped
/// to the line), UTF-16/UTF-32 need column math. Clamped to the line length so a
/// past-end character (a diagnostic whose range runs to EOL) lands at the end.
fn byte_col(encoding: PositionEncoding, line: &str, character: usize) -> usize {
    match encoding {
        PositionEncoding::Utf8 => character.min(line.len()),
        PositionEncoding::Utf16 => unicode::utf16_to_byte(line, character),
        PositionEncoding::Utf32 => line
            .char_indices()
            .nth(character)
            .map_or(line.len(), |(i, _)| i),
    }
}

/// Map an LSP [`DiagnosticSeverity`] to nxvim's severity code (`1`=error,
/// `2`=warning, `3`=info, `4`=hint). An absent severity is treated as an error,
/// matching how most servers and neovim render an unspecified diagnostic. The
/// constants aren't enum variants (lsp-types models them as a newtype), so this
/// compares rather than pattern-matches.
fn severity_code(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(s) if s == DiagnosticSeverity::WARNING => 2,
        Some(s) if s == DiagnosticSeverity::INFORMATION => 3,
        Some(s) if s == DiagnosticSeverity::HINT => 4,
        _ => 1,
    }
}

/// The highlight group whose `sp`/underline style paints a diagnostic of this
/// severity code, resolved through the registry like the chrome groups.
fn severity_group(severity: u8) -> &'static str {
    match severity {
        2 => "DiagnosticUnderlineWarn",
        3 => "DiagnosticUnderlineInfo",
        4 => "DiagnosticUnderlineHint",
        _ => "DiagnosticUnderlineError",
    }
}

/// One-letter severity tag for the location-list column (`E`/`W`/`I`/`H`).
fn severity_short(severity: u8) -> char {
    match severity {
        2 => 'W',
        3 => 'I',
        4 => 'H',
        _ => 'E',
    }
}

/// The first non-empty line of a (possibly multi-line, markdown) diagnostic
/// message, for the single-line message line and location-list rows.
fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Human label for a document-sync kind.
fn sync_label(kind: TextDocumentSyncKind) -> &'static str {
    match kind {
        TextDocumentSyncKind::FULL => "full",
        TextDocumentSyncKind::INCREMENTAL => "incremental",
        _ => "none",
    }
}

/// The built-in filetype→server-command table (Decision 6), the `filetype_of`
/// analogue. A real server is used only if its binary is on `PATH`. The
/// `NXVIM_LSP_CMD` env var overrides the whole table with a single command (the
/// mock, in tests) — the LSP analogue of `NXVIM_TS_WORKER`.
pub(crate) fn lsp_spawn_for(language: &str) -> Option<ServerSpawn> {
    if let Ok(cmd) = std::env::var("NXVIM_LSP_CMD") {
        let mut parts = cmd.split_whitespace().map(str::to_string);
        let program = parts.next()?;
        return Some(ServerSpawn {
            program,
            args: parts.collect(),
        });
    }
    let (program, args): (&str, &[&str]) = match language {
        "rust" => ("rust-analyzer", &[]),
        "python" => ("pyright-langserver", &["--stdio"]),
        "go" => ("gopls", &[]),
        "lua" => ("lua-language-server", &[]),
        _ => return None,
    };
    if !binary_on_path(program) {
        return None;
    }
    Some(ServerSpawn {
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
    })
}

/// Whether `program` resolves to a file on `PATH` (gating server auto-start on
/// the binary actually being installed).
fn binary_on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// The workspace root for a file: its absolute parent directory. Phase 1 uses
/// the containing directory directly; root-marker search (`Cargo.toml`, `.git`,
/// …) is a later refinement.
fn workspace_root(path: &Path) -> PathBuf {
    let abs = absolutize(path);
    abs.parent().map(Path::to_path_buf).unwrap_or(abs)
}

/// A `file://` URI for an absolute-ized path, or `None` if it can't be formed.
pub(crate) fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(absolutize(path)).ok()
}

/// Resolve `path` against the current directory if it is relative.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
