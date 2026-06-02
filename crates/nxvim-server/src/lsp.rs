//! Server-side LSP integration: the `syntax.rs` analogue for language servers.
//!
//! Where `nxvim-lsp` owns the client machinery (spawning/supervising servers and
//! the JSON-RPC bridge — the `SyntaxClient` analogue), this module owns the
//! *editor* half: the built-in filetype→server config table, per-buffer
//! document-sync bookkeeping ([`LspDocState`], keyed by [`BufferId`] like
//! `SyntaxState`), byte↔LSP position conversion via `nxvim-core`'s pure unicode
//! helpers, and the handling of [`LspEvent`]s. Document sync reuses the buffer
//! edit journal (`take_edits`/`changedtick`) the syntax sync already drives, so
//! no new core machinery is added (Decision 5).
//!
//! All [`Server`] methods here run on the single editor thread and only ever
//! hand the manager fire-and-forget [`LspNotify`]s, so a slow or hung server can
//! never stall keystroke→buffer→redraw.

use std::path::{Path, PathBuf};

use nxvim_core::unicode;
use nxvim_core::{BufferEdit, BufferId};
use nxvim_lsp::lsp_types::{
    Diagnostic, Position, Range, TextDocumentContentChangeEvent, TextDocumentSyncKind, Url,
};
use nxvim_lsp::{LspEvent, LspNotify, PositionEncoding, ServerKey, ServerSpawn};

use crate::{filetype_of, Server};

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
    /// `modified` as of the last sync, to detect a `:w` (modified cleared with no
    /// text change ⇒ a write, not an undo-to-clean).
    was_modified: bool,
    /// Latest `publishDiagnostics` for this buffer (cached in Phase 1; projected
    /// to the redraw in Phase 2).
    #[allow(dead_code)]
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
        let cur_modified = self.editor.buffer().modified;

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

        // Save: `modified` cleared with no text change since last observation is a
        // `:w` (an undo-to-clean would have bumped `changedtick`, i.e.
        // `tick_changed`).
        if state.was_modified && !cur_modified && !tick_changed {
            self.lsp.notify(key, LspNotify::DidSave { uri, text: None });
        }
        state.was_modified = cur_modified;

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
                // Phase 1 only caches; Phase 2 projects these into the redraw.
                if let Some(state) = self
                    .lsp_states
                    .values_mut()
                    .find(|s| s.uri.as_ref() == Some(&uri))
                {
                    state.diagnostics = diagnostics;
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
