//! The persistence (shada) snapshot seam.
//!
//! `nxvim-core` stays pure and synchronous, so it never touches the shada file
//! itself. Instead it exposes a plain, owned [`PersistState`] — the cross-session
//! state worth saving — through [`Editor::export_persist`] and seeds it back with
//! [`Editor::import_persist`]. The server (`nxvim-server/src/shada.rs`) owns every
//! byte of I/O: it serializes this struct into a per-instance redb store, merges
//! sibling stores on load, and stamps the merge timestamps. Keeping the timestamp
//! and the storage out of here is deliberate — they are the *server's* merge
//! concern, not the editor model's.
//!
//! Phase 1 carries **registers**; Phase 2 adds the global file marks `A`–`Z`.
//! Later phases grow the struct with per-file / numbered marks, the
//! jumplist/changelist, and history. See
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::PathBuf;

use super::registers::RegKind;
use super::{Cursor, Editor};

/// The cross-session editor state a shada store persists. Plain owned data with
/// no timestamps (the server stamps those at write time, since recency is its
/// merge key) and no `BufferId`s (positions resolve through file paths, which
/// survive a restart where session-local ids do not).
#[derive(Debug, Clone, Default)]
pub struct PersistState {
    /// The register file: named `"a`–`"z`, numbered `"0`–`"9`, the unnamed `"`,
    /// and small-delete `"-`. The black hole and the live-resolved specials
    /// (`"%` `".` `":` `"/` `"+` `"*`) are never stored.
    pub registers: Vec<RegisterEntry>,
    /// The global file marks `A`–`Z`, each as a `(path, line, col)`. Positions
    /// store a **path** (not a session-local `BufferId`) so they resolve across a
    /// restart; on import they seed [`Editor::pending_global_marks`] and the file
    /// opens lazily on the first jump.
    pub global_marks: Vec<GlobalMarkEntry>,
}

/// One persisted global mark: its name (`A`–`Z`), the file it points into, and
/// the 0-based `(line, col)` within that file. The path replaces the live
/// `BufferId` (meaningless across sessions); restoring re-resolves it.
#[derive(Debug, Clone)]
pub struct GlobalMarkEntry {
    pub name: char,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

/// One persisted register: its name, contents, and how it pastes.
#[derive(Debug, Clone)]
pub struct RegisterEntry {
    pub name: char,
    pub text: String,
    /// `true` if the register pastes linewise (vim's `RegKind::Line`), `false`
    /// for charwise. A plain bool rather than the crate-private `RegKind` so the
    /// snapshot type carries no internal enum across the crate boundary.
    pub linewise: bool,
}

impl Editor {
    /// Snapshot the cross-session state into a [`PersistState`] for the server to
    /// write. Pure: reads live editor state, allocates owned copies, touches no
    /// I/O.
    pub fn export_persist(&self) -> PersistState {
        let registers = self
            .registers
            .entries()
            .into_iter()
            .map(|(name, text, kind)| RegisterEntry {
                name,
                text: text.to_string(),
                linewise: kind == RegKind::Line,
            })
            .collect();
        PersistState {
            registers,
            global_marks: self.export_global_marks(),
        }
    }

    /// Resolve the global marks `A`–`Z` to `(path, line, col)` for persistence.
    /// A *live* mark's `BufferId` resolves to its file path (an unnamed buffer —
    /// empty path — is dropped, having nothing to reopen); a mark still *pending*
    /// from a previous restore (its file never reopened this session) carries its
    /// stored path straight through, so an untouched restored mark survives the
    /// next save too.
    fn export_global_marks(&self) -> Vec<GlobalMarkEntry> {
        let mut marks: Vec<GlobalMarkEntry> = self
            .global_marks
            .iter()
            .filter_map(|(&name, &(buf, cursor))| {
                let path = self.buffer_name(buf).filter(|p| !p.is_empty())?;
                Some(GlobalMarkEntry {
                    name,
                    path: PathBuf::from(path),
                    line: cursor.line,
                    col: cursor.col,
                })
            })
            .collect();
        for (&name, (path, cursor)) in &self.pending_global_marks {
            // A live mark of the same name (re-set this session) wins over the
            // stale pending one.
            if self.global_marks.contains_key(&name) {
                continue;
            }
            marks.push(GlobalMarkEntry {
                name,
                path: path.clone(),
                line: cursor.line,
                col: cursor.col,
            });
        }
        marks
    }

    /// Seed editor state from a (merged) [`PersistState`] the server loaded.
    /// Called once at startup before the first frame. Additive — it fills empty
    /// slots; it does not clear state the running session has already set.
    pub fn import_persist(&mut self, state: PersistState) {
        for entry in state.registers {
            let kind = if entry.linewise {
                RegKind::Line
            } else {
                RegKind::Char
            };
            self.registers.set_api(entry.name, entry.text, kind, false);
        }
        // Global marks seed the *pending* map, not the live one: the marked file
        // is not opened until the first `` `A `` jump (vim never bulk-loads marked
        // files at startup). Additive — a mark the running session has already set
        // live is not overwritten by the restored one.
        for entry in state.global_marks {
            if self.global_marks.contains_key(&entry.name) {
                continue;
            }
            self.pending_global_marks.insert(
                entry.name,
                (
                    entry.path,
                    Cursor {
                        line: entry.line,
                        col: entry.col,
                    },
                ),
            );
        }
    }
}
