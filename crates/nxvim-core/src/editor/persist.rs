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
//! Phase 1 carries **registers**; Phase 2 the global file marks `A`–`Z`; Phase 3
//! the per-file marks (`a`–`z`, specials, the `"` last-cursor) and search/ex
//! history; Phase 4 the numbered marks `'0`–`'9`, the per-file changelist, the
//! focused window's jumplist, and the clean-exit cursor that seeds `'0`. See
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::PathBuf;

use super::registers::RegKind;
use super::{Cursor, Editor};

/// The cross-session editor state a shada store persists. Plain owned data with
/// no timestamps (the server stamps those at write time, since recency is its
/// merge key) and no `BufferId`s (positions resolve through file paths, which
/// survive a restart where session-local ids do not).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
    /// Per-file marks: the buffer-local `a`–`z`, the automatic specials, and the
    /// `"` last-cursor mark, each keyed by the file it lives in. Restored when the
    /// file is reopened, so `` `" `` lands where the file was last left.
    pub file_marks: Vec<FileMarkEntry>,
    /// The search (`/`) history, oldest entry first.
    pub search_history: Vec<String>,
    /// The ex command-line (`:`) history, oldest entry first.
    pub ex_history: Vec<String>,
    /// The numbered marks `'0`–`'9` (digit `'0'`–`'9'`), each a `(path, line,
    /// col)`. A pure persistence construct — the *store* shifts them at load
    /// (`'0` ← last exit cursor, old `'0`→`'1`, …) — so core only seeds whatever
    /// the store hands it.
    pub numbered_marks: Vec<NumberedMark>,
    /// Per-file changelists (the `g;`/`g,` history), keyed by path, restored when
    /// the file is reopened.
    pub file_changelists: Vec<FileChangelist>,
    /// The focused window's jumplist (`<C-o>`/`<C-i>`), oldest entry first, each a
    /// `(path, line, col)`.
    pub jumplist: Vec<JumpPos>,
    /// Where the cursor sat at the last *clean* exit. Written only by the
    /// exit-flush (not the carry-forward flush), and consumed by the store on the
    /// next load to become `'0`. `None` in a merged snapshot (already consumed).
    pub exit_cursor: Option<JumpPos>,
}

/// One persisted numbered mark `'0`–`'9`: the digit, the file, and the position.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NumberedMark {
    pub digit: char,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

/// One persisted per-file changelist: the file and its `(line, col)` change
/// positions, oldest first.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileChangelist {
    pub path: PathBuf,
    pub entries: Vec<(usize, usize)>,
}

/// One position in a persisted jumplist or the exit cursor: a file path and a
/// 0-based `(line, col)`.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JumpPos {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

/// One persisted global mark: its name (`A`–`Z`), the file it points into, and
/// the 0-based `(line, col)` within that file. The path replaces the live
/// `BufferId` (meaningless across sessions); restoring re-resolves it.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GlobalMarkEntry {
    pub name: char,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

/// One persisted per-file mark: the file it lives in, the mark name (`a`–`z`, a
/// special, or `"`), and the 0-based `(line, col)` within that file.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileMarkEntry {
    pub path: PathBuf,
    pub name: char,
    pub line: usize,
    pub col: usize,
}

/// A deferred shada I/O request raised by `:wshada` / `:rshada`. Core can't touch
/// the store (it lives in the server, behind the `ShadaStore` seam), so the
/// ex-command enqueues one of these and the server drains it after the tick — the
/// same core→server hand-off [`PendingSave`](super::PendingSave) / `pending_checktime`
/// use. Phase 7 (`docs/plans/2026-06-11-shada-persistence.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadaRequest {
    /// `:wshada` — flush this instance's store now (a synchronous, explicit
    /// checkpoint, like `:w`). Never writes the clean-exit cursor: `'0` tracks
    /// *exits* only, and `:wshada` is not one.
    Write,
    /// `:rshada` / `:rshada!` — re-read the store(s) into the running session. The
    /// store re-merges every *readable* sibling (a still-live instance's file is
    /// locked, hence invisible — neovim's contract) plus this instance's own. When
    /// `replace` (the `!`) is set, a stored value overwrites a conflicting live one;
    /// otherwise it only fills an empty slot.
    Read { replace: bool },
}

/// One persisted register: its name, contents, and how it pastes.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
            file_marks: self.export_file_marks(),
            search_history: self.search_history.clone(),
            ex_history: self.ex_history.clone(),
            numbered_marks: self.export_numbered_marks(),
            file_changelists: self.export_changelists(),
            jumplist: self.export_jumplist(),
            exit_cursor: self.export_exit_cursor(),
        }
    }

    /// The numbered marks `'0`–`'9` as `(digit, path, line, col)`. They never
    /// change during a session (the store shifts them at load), so this just hands
    /// back what was seeded so the next save carries them forward.
    fn export_numbered_marks(&self) -> Vec<NumberedMark> {
        self.numbered_marks
            .iter()
            .map(|(&digit, (path, cursor))| NumberedMark {
                digit,
                path: path.clone(),
                line: cursor.line,
                col: cursor.col,
            })
            .collect()
    }

    /// Each named open buffer's changelist (keyed by path), plus any restored
    /// changelist for a file not reopened this session (carried forward).
    fn export_changelists(&self) -> Vec<FileChangelist> {
        let mut out: Vec<FileChangelist> = Vec::new();
        for ob in self.buffers.map.values() {
            let Some(path) = ob
                .buffer
                .path
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
            else {
                continue;
            };
            if ob.buffer.changelist.is_empty() {
                continue;
            }
            out.push(FileChangelist {
                path: path.clone(),
                entries: ob.buffer.changelist.clone(),
            });
        }
        for (path, entries) in &self.pending_changelists {
            out.push(FileChangelist {
                path: path.clone(),
                entries: entries.clone(),
            });
        }
        out
    }

    /// The focused window's jumplist as `(path, line, col)`, resolving each entry's
    /// `BufferId` to a file path (an entry in an unnamed buffer is dropped — there
    /// is nothing to reopen). If the restored jumplist was never materialized this
    /// session, carry it forward untouched.
    fn export_jumplist(&self) -> Vec<JumpPos> {
        if self.windows.cur().jumps.is_empty() && !self.pending_jumplist.is_empty() {
            return self
                .pending_jumplist
                .iter()
                .map(|(path, line, col)| JumpPos {
                    path: path.clone(),
                    line: *line,
                    col: *col,
                })
                .collect();
        }
        self.windows
            .cur()
            .jumps
            .iter()
            .filter_map(|e| {
                let path = self.buffer_name(e.buf).filter(|p| !p.is_empty())?;
                Some(JumpPos {
                    path: PathBuf::from(path),
                    line: e.line,
                    col: e.col,
                })
            })
            .collect()
    }

    /// Where the cursor sits now, as the *clean-exit* cursor the store turns into
    /// `'0` next launch. `None` for an unnamed current buffer (nothing to reopen).
    fn export_exit_cursor(&self) -> Option<JumpPos> {
        let path = self
            .buffer_name(self.cur_buffer())
            .filter(|p| !p.is_empty())?;
        Some(JumpPos {
            path: PathBuf::from(path),
            line: self.cursor.line,
            col: self.cursor.col,
        })
    }

    /// Resolve the per-file marks of every named open buffer — plus any restored
    /// marks for files not reopened this session — to `(path, name, line, col)`.
    /// The *current* buffer's live cursor is stamped as its `"` last-cursor mark
    /// (it is never "left", so its stored `"` would be stale), so reopening it next
    /// session lands at the spot the editor was quit from.
    fn export_file_marks(&self) -> Vec<FileMarkEntry> {
        let mut out = Vec::new();
        let current = self.cur_buffer();
        for (&id, ob) in &self.buffers.map {
            let Some(path) = ob
                .buffer
                .path
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
            else {
                continue;
            };
            let mut marks = ob.buffer.marks.clone();
            if id == current {
                marks.insert('"', (self.cursor.line, self.cursor.col));
            }
            for (name, (line, col)) in marks {
                out.push(FileMarkEntry {
                    path: path.clone(),
                    name,
                    line,
                    col,
                });
            }
        }
        // Files marked in a previous session but never reopened in this one keep
        // their restored marks so the next save carries them forward too.
        for (path, marks) in &self.pending_file_marks {
            for (&name, &(line, col)) in marks {
                out.push(FileMarkEntry {
                    path: path.clone(),
                    name,
                    line,
                    col,
                });
            }
        }
        out
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

    /// Drain the deferred shada requests (`:wshada` / `:rshada`) raised this tick,
    /// for the server to act on against its store. Empty (a cheap clone of nothing)
    /// when neither command ran.
    pub fn take_pending_shada(&mut self) -> Vec<ShadaRequest> {
        std::mem::take(&mut self.pending_shada)
    }

    /// Seed editor state from a (merged) [`PersistState`] the server loaded.
    /// Called once at startup before the first frame. Additive — it fills empty
    /// slots; it does not clear state the running session has already set.
    pub fn import_persist(&mut self, state: PersistState) {
        self.apply_persist(state, false);
    }

    /// Apply a (merged) [`PersistState`], either filling only empty slots
    /// (`replace = false`, the startup load and a plain `:rshada`) or overwriting a
    /// conflicting live value (`replace = true`, `:rshada!`). The only state with a
    /// genuine *conflict* is the register file — a register the running session has
    /// already set; everything else (marks, history, jumplist, changelist) is seeded
    /// through the lazy pending-by-path maps, which are inherently additive (a
    /// re-set mark already wins on export), so `replace` does not affect them.
    pub fn apply_persist(&mut self, state: PersistState, replace: bool) {
        for entry in state.registers {
            // A live register set this session is a conflict: keep it unless the
            // bang (`replace`) says to overwrite.
            if !replace && self.registers.get(Some(entry.name)).is_some() {
                continue;
            }
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
        // Per-file marks seed the pending-by-path map keyed *normalized* (so the
        // lookup at buffer-load matches regardless of how the path is spelled),
        // then the already-open startup buffer is seeded immediately — later opens
        // pick theirs up as the file loads.
        for entry in state.file_marks {
            self.pending_file_marks
                .entry(super::normalize_path(&entry.path))
                .or_default()
                .entry(entry.name)
                .or_insert((entry.line, entry.col));
        }
        // Per-file changelists seed the same pending-by-path map the marks use, so
        // a reopened file gets its `g;`/`g,` history back when it loads.
        for entry in state.file_changelists {
            self.pending_changelists
                .entry(super::normalize_path(&entry.path))
                .or_insert(entry.entries);
        }
        let cur = self.cur_buffer();
        self.seed_pending_file_marks(cur);
        // History restored from disk is older than anything typed this session;
        // merge it *ahead* of the (empty, at startup) live history, dropping older
        // duplicates so a repeated entry keeps its newest position.
        merge_history(&mut self.search_history, state.search_history);
        merge_history(&mut self.ex_history, state.ex_history);
        // Numbered marks `'0`–`'9` were already shifted by the store at load; seed
        // them path-based (resolved to a buffer lazily on the `` `0 `` jump).
        for entry in state.numbered_marks {
            self.numbered_marks.insert(
                entry.digit,
                (
                    entry.path,
                    Cursor {
                        line: entry.line,
                        col: entry.col,
                    },
                ),
            );
        }
        // The jumplist waits as pending paths; the first `<C-o>` materializes it
        // (opening the files). `exit_cursor` is consumed by the store into `'0`, so
        // a merged snapshot carries none — nothing to import here.
        self.pending_jumplist = state
            .jumplist
            .into_iter()
            .map(|j| (j.path, j.line, j.col))
            .collect();
    }
}

/// Fold `restored` (older) history in front of `live` (newer), de-duplicating by
/// text so a repeated entry survives only at its most-recent position. At startup
/// `live` is empty, so this is just the restored list with dups collapsed.
fn merge_history(live: &mut Vec<String>, restored: Vec<String>) {
    let mut merged = restored;
    for entry in live.drain(..) {
        merged.retain(|e| e != &entry);
        merged.push(entry);
    }
    *live = merged;
}
