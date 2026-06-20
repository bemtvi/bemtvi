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
    /// The per-workspace **session**: the open files, their cursor / scroll, and
    /// the tab + split layout, restored at boot for a session-scoped workspace.
    /// `None` for the global store — only the server attaches it (via
    /// [`Editor::export_session`]) when a workspace namespace is active, so a
    /// non-workspace shada never carries layout. See `docs/architecture.md`.
    pub session: Option<SessionState>,
}

/// A captured editor **session**: every tab's EXACT split layout (nesting + sizes) with
/// the open file + view at each leaf, plus which tab was focused. Restored eagerly at
/// boot ([`Editor::restore_session`]). Window ids are deliberately NOT stored — they're
/// session-local and reminted on restore; positions resolve through file paths.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionState {
    /// The tab pages, in tabline order. A tab with no file-backed window is dropped.
    pub tabs: Vec<SessionTab>,
    /// Index into [`SessionState::tabs`] of the tab that was focused.
    pub active_tab: usize,
    /// The edge docks open at capture (left/right/top/bottom), each with its size,
    /// hidden state, and any file-backed content. Empty when no dock was open.
    pub docks: Vec<SessionDock>,
}

/// One edge dock: which side, its reserved size in cells (the boot-time fallback), an
/// optional `size_pct` (the size as a percentage of the screen, when captured with
/// `relative_docks` so the dock scales), whether it was hidden (parked), and its
/// file-backed window layout if any (plugin-owned docks — file trees, terminals — show
/// unnamed buffers, so their `layout` is `None` and they reopen empty at the saved
/// geometry for the owning plugin to repopulate).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionDock {
    pub side: String,
    pub size: usize,
    pub size_pct: Option<usize>,
    pub hidden: bool,
    pub layout: Option<SessionLayout>,
}

/// One tab page: its split layout tree.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionTab {
    pub layout: SessionLayout,
}

/// The split layout of a tab: a `Leaf` window, or a `Split` dividing its area among
/// `children` (`vertical` = side-by-side columns; `sizes` are the proportional weights
/// the restore re-lays-out). Mirrors the window model's private layout tree.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SessionLayout {
    Leaf(SessionWindow),
    Split {
        vertical: bool,
        sizes: Vec<usize>,
        children: Vec<SessionLayout>,
    },
}

/// One window: the file it showed, its view (cursor line/col + top line), and whether it
/// was the tab's focused window. Only file-backed windows are captured (a path is
/// required to reopen the buffer).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionWindow {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub top: usize,
    pub active: bool,
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
            // The session rides the store only for a namespaced workspace; the server
            // attaches it via export_session() when enabled, so export_persist leaves
            // it None (the global shada never carries layout).
            session: None,
        }
    }

    /// Capture the current tab + EXACT split layout for a workspace session: each tab's
    /// nesting + proportional sizes, with the file path / cursor / scroll at every leaf
    /// and the focused window marked. Floating windows and unnamed (scratch) buffers are
    /// dropped (single-child splits collapse). Returns `None` when nothing is worth
    /// saving. Pure: reads live state, no I/O.
    /// The native `'relative_splits'` option (default on) stores split sizes as
    /// proportional percentages rather than absolute cells; `'relative_docks'` (default
    /// off) likewise stores a dock's size as a percentage of the screen so it scales.
    /// Both are read straight off [`Options`](crate::options::Options) here, so any
    /// wrapper that opts a session into capture honors them — they are not coupled to
    /// any one plugin.
    pub fn export_session(&self) -> Option<SessionState> {
        let relative_splits = self.options.relative_splits;
        let current_tab = self.current_tab_id();
        let mut tabs = Vec::new();
        let mut active_tab = 0;
        for tid in self.tab_ids() {
            let node = match self.tab_layout_node(tid) {
                Some(n) => n,
                None => continue,
            };
            let active = self.tab_current_window(tid);
            let layout = match self.capture_layout(&node, active, relative_splits) {
                Some(l) => l,
                None => continue, // no file-backed leaf in this tab
            };
            if tid == current_tab {
                active_tab = tabs.len();
            }
            tabs.push(SessionTab { layout });
        }
        let docks = self.export_docks();
        if tabs.is_empty() && docks.is_empty() {
            return None;
        }
        Some(SessionState {
            tabs,
            active_tab,
            docks,
        })
    }

    /// Capture every open edge dock: its side, size (cells, plus a `size_pct` percentage
    /// when `relative_docks`), hidden state, and any file-backed content. A plugin-owned
    /// dock (unnamed buffer) records `layout = None` and reopens empty at the geometry.
    fn export_docks(&self) -> Vec<SessionDock> {
        use super::{DockSide, Layer};
        let relative_splits = self.options.relative_splits;
        let relative_docks = self.options.relative_docks;
        let mut docks = Vec::new();
        for side in DockSide::ALL {
            if !self.dock_exists(side) {
                continue;
            }
            let layout = self.layer_tree(Layer::Dock(side)).and_then(|t| {
                self.capture_layout(&t.layout_node(), Some(t.current), relative_splits)
            });
            let cells = self.dock_option_values(side.keyword()).2;
            // A left/right dock is a fraction of the screen width; top/bottom of height.
            let screen = if matches!(side, DockSide::Left | DockSide::Right) {
                self.width
            } else {
                self.height
            };
            let size_pct = if relative_docks && screen > 0 {
                Some((cells * 100 / screen).max(1))
            } else {
                None
            };
            docks.push(SessionDock {
                side: side.keyword().to_string(),
                size: cells,
                size_pct,
                hidden: !self.dock_is_open(side),
                layout,
            });
        }
        docks
    }

    /// Project a window-model [`LayoutNode`] into a serialisable [`SessionLayout`],
    /// resolving each leaf window to its file + view. Leaves with no file (unnamed /
    /// scratch) are dropped, and a split left with one child collapses to that child;
    /// `None` when nothing file-backed survives. With `relative_splits` the kept sizes are
    /// normalized to percentages (summing ~100) — the restore re-lays them out either way,
    /// since the window model treats split sizes as proportional weights.
    fn capture_layout(
        &self,
        node: &super::windows::LayoutNode,
        active: Option<crate::WindowId>,
        relative_splits: bool,
    ) -> Option<SessionLayout> {
        use super::windows::LayoutNode;
        match node {
            LayoutNode::Leaf(wid) => {
                let buf = self.window_buffer(*wid)?;
                let path = self.buffer_name(buf).unwrap_or_default();
                if path.is_empty() {
                    return None;
                }
                let (line, col) = self.window_cursor(*wid).unwrap_or((0, 0));
                let (top, _leftcol) = self.window_scroll(*wid).unwrap_or((0, 0));
                Some(SessionLayout::Leaf(SessionWindow {
                    path: PathBuf::from(path),
                    line,
                    col,
                    top,
                    active: active == Some(*wid),
                }))
            }
            LayoutNode::Split {
                vertical,
                sizes,
                children,
            } => {
                let mut kids = Vec::new();
                let mut kept_sizes = Vec::new();
                for (i, child) in children.iter().enumerate() {
                    if let Some(sl) = self.capture_layout(child, active, relative_splits) {
                        kids.push(sl);
                        kept_sizes.push(sizes.get(i).copied().unwrap_or(1));
                    }
                }
                match kids.len() {
                    0 => None,
                    1 => kids.into_iter().next(),
                    _ => Some(SessionLayout::Split {
                        vertical: *vertical,
                        sizes: if relative_splits {
                            to_percentages(&kept_sizes)
                        } else {
                            kept_sizes
                        },
                        children: kids,
                    }),
                }
            }
        }
    }

    /// Rebuild a saved [`SessionState`] at boot, EXACTLY: each tab's split tree (nesting,
    /// orientation, proportional sizes) is reconstructed, the files reopened at their
    /// saved cursor + scroll, and the saved tab / window refocused. The first tab reuses
    /// the startup tab; later tabs are fresh pages. Files that no longer open are skipped
    /// (their split collapses). A no-op for an empty session.
    pub fn restore_session(&mut self, session: SessionState) {
        use crate::options::WindowOptions;
        use crate::WindowId;
        use std::collections::BTreeMap;
        if session.tabs.is_empty() && session.docks.is_empty() {
            return;
        }
        let mut built_any = false;
        for tab in &session.tabs {
            let mut windows: BTreeMap<WindowId, super::windows::Window> = BTreeMap::new();
            let mut active: Option<WindowId> = None;
            let root = match self.build_layout(&tab.layout, &mut windows, &mut active) {
                Some(r) => r,
                None => continue,
            };
            let current = match active.or_else(|| windows.keys().next().copied()) {
                Some(c) => c,
                None => continue,
            };
            let tree = super::windows::WindowTree::from_layout(windows, root, current);
            let buf = tree.get(current).buffer;
            // Tab 0 reuses the startup tab's tree; later tabs get a fresh page first.
            if built_any {
                self.new_tab(buf, WindowOptions::default());
            }
            built_any = true;
            self.install_restored_tree(tree);
        }
        // Restore the edge docks (each `open_dock` makes the dock the live layer, so the
        // rebuilt content installs the same way a main tab does).
        self.restore_docks(&session.docks);
        // Focus the saved active tab — this crosses back to the main layer, parking the
        // docks and leaving the editor focused where it was.
        let tab_ids = self.tab_ids();
        if let Some(tid) = tab_ids.get(session.active_tab.min(tab_ids.len().saturating_sub(1))) {
            self.set_current_tabpage(*tid);
        }
    }

    /// Reopen each saved [`SessionDock`] at its side + size, rebuilding any file-backed
    /// content (a plugin dock reopens empty for its owner to repopulate) and re-hiding a
    /// dock that was parked.
    fn restore_docks(&mut self, docks: &[SessionDock]) {
        use crate::WindowId;
        use std::collections::BTreeMap;
        for d in docks {
            let side = match super::DockSide::from_keyword(&d.side) {
                Some(s) => s,
                None => continue,
            };
            // A relative dock (captured as a % of the screen) re-derives its cells from
            // the live screen size; when that isn't known yet (restore runs before the UI
            // attaches), fall back to the captured cell size.
            let size = match d.size_pct {
                Some(pct) => {
                    let screen = if matches!(side, super::DockSide::Left | super::DockSide::Right) {
                        self.width
                    } else {
                        self.height
                    };
                    if screen > 0 {
                        (pct * screen / 100).max(1)
                    } else {
                        d.size
                    }
                }
                None => d.size,
            };
            let rebuilt = d.layout.as_ref().and_then(|layout| {
                let mut windows: BTreeMap<WindowId, super::windows::Window> = BTreeMap::new();
                let mut active: Option<WindowId> = None;
                let root = self.build_layout(layout, &mut windows, &mut active)?;
                let current = active.or_else(|| windows.keys().next().copied())?;
                Some(super::windows::WindowTree::from_layout(
                    windows, root, current,
                ))
            });
            match rebuilt {
                Some(tree) => {
                    let buf = tree.get(tree.current).buffer;
                    self.open_dock(side, size, Some(buf)); // dock is now the live layer
                    self.install_restored_tree(tree); // swap in the rebuilt dock tree
                }
                None => self.open_dock(side, size, None), // empty dock at saved geometry
            }
            if d.hidden {
                self.hide_dock(side);
            }
        }
    }

    /// Recursively realise a [`SessionLayout`] into a window map + a [`LayoutNode`]
    /// skeleton: open each leaf's file (minting a fresh window id), drop leaves whose
    /// file is gone, and collapse a split left with one child. `None` if nothing opens.
    fn build_layout(
        &mut self,
        layout: &SessionLayout,
        windows: &mut std::collections::BTreeMap<crate::WindowId, super::windows::Window>,
        active: &mut Option<crate::WindowId>,
    ) -> Option<super::windows::LayoutNode> {
        use super::windows::{LayoutNode, WindowTree};
        match layout {
            SessionLayout::Leaf(w) => {
                let buf = self.open_buffer(&w.path)?;
                let id = self.alloc_window_id();
                let cursor = Cursor {
                    line: w.line,
                    col: w.col,
                };
                windows.insert(id, WindowTree::tiled_window(buf, cursor, w.top, 0));
                if w.active {
                    *active = Some(id);
                }
                Some(LayoutNode::Leaf(id))
            }
            SessionLayout::Split {
                vertical,
                sizes,
                children,
            } => {
                let mut kids = Vec::new();
                let mut kept_sizes = Vec::new();
                for (i, child) in children.iter().enumerate() {
                    if let Some(node) = self.build_layout(child, windows, active) {
                        kids.push(node);
                        kept_sizes.push(sizes.get(i).copied().unwrap_or(1));
                    }
                }
                match kids.len() {
                    0 => None,
                    1 => kids.into_iter().next(),
                    _ => Some(LayoutNode::Split {
                        vertical: *vertical,
                        sizes: kept_sizes,
                        children: kids,
                    }),
                }
            }
        }
    }

    /// Install a freshly rebuilt [`WindowTree`] as the active tab's layout, syncing the
    /// editor's live buffer + cursor + view to its focused window and laying it out.
    fn install_restored_tree(&mut self, tree: super::windows::WindowTree) {
        let current = tree.current;
        let (buf, sc, st, sl) = {
            let w = tree.get(current);
            (w.buffer, w.saved_cursor, w.saved_top, w.saved_leftcol)
        };
        self.windows = tree;
        self.set_cur_buffer(buf);
        self.cursor = sc;
        self.top = st;
        self.leftcol = sl;
        self.relayout();
        self.clamp_cursor();
        self.ensure_visible();
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

/// Normalize split weights to percentages summing to ~100 (the last child absorbs the
/// rounding remainder so the parts always add up). The window model treats split sizes
/// as proportional weights, so this only changes the STORED representation — a 30/70
/// split reads back as `[30, 70]` instead of, say, `[24, 56]` cells.
fn to_percentages(sizes: &[usize]) -> Vec<usize> {
    let total: usize = sizes.iter().sum::<usize>().max(1);
    let mut out: Vec<usize> = sizes.iter().map(|s| s * 100 / total).collect();
    let sum: usize = out.iter().sum();
    if let Some(last) = out.last_mut() {
        *last += 100usize.saturating_sub(sum); // hand the remainder to the last part
    }
    out
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
