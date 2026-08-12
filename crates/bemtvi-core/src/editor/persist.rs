//! The persistence (shada) snapshot seam.
//!
//! `bemtvi-core` stays pure and synchronous, so it never touches the shada file
//! itself. Instead it exposes a plain, owned [`PersistState`] — the cross-session
//! state worth saving — through [`Editor::export_persist`] and seeds it back with
//! [`Editor::import_persist`]. The server (`bemtvi-server/src/shada.rs`) owns every
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
    /// The per-namespace `btv.ui.input{ history = "<ns>" }` rings (e.g. the DAP repl's
    /// `dap>` prompt recall), each oldest entry first. One entry per namespace; routed
    /// to the same store and gated by the same `'persisthistory'` scope as the `:` / `/`
    /// histories above. `#[serde(default)]` so an older store without it loads as none.
    #[cfg_attr(feature = "serde", serde(default))]
    pub input_history: Vec<InputHistoryEntry>,
    /// The numbered marks `'0`–`'9` (digit `'0'`–`'9'`), each a `(path, line,
    /// col)`. A pure persistence construct — the *store* shifts them at load
    /// (`'0` ← last exit cursor, old `'0`→`'1`, …) — so core only seeds whatever
    /// the store hands it.
    pub numbered_marks: Vec<NumberedMark>,
    /// Per-file changelists (the `g;`/`g,` history), keyed by path, restored when
    /// the file is reopened.
    pub file_changelists: Vec<FileChangelist>,
    /// Per-file **manual** folds, keyed by path, restored into the window that
    /// reopens the file (vim's `:mkview`-style fold persistence in shada).
    pub file_folds: Vec<FileFolds>,
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
    /// Per-plugin **isolated** key/value data: each entry is one plugin
    /// namespace's full key→value map. A pure *transport* field — `bemtvi-core`
    /// never reads or writes it (the data lives in the Lua runtime, not the editor
    /// model). The server fills it from the runtime at flush and seeds it back at
    /// load, keyed under a namespace so a plugin can only reach its own slice and
    /// never the core registers / marks / history. Empty for a session with no
    /// opted-in plugin. See `docs/plans/2026-06-26-plugin-shada-namespaces.md`.
    pub plugin_data: Vec<PluginNamespace>,
    /// The per-workspace **option overlay** (`btv.wso`): canonical global-option name → the
    /// workspace's overriding value, which wins over the process-global value while the
    /// workspace is open. Captured only for a workspace-scoped session (the server attaches
    /// it like [`PersistState::session`]) and re-applied at load via
    /// [`Editor::seed_workspace_options`]. `#[serde(default)]` so an older store without it
    /// loads as no overrides. See [`crate::options::WorkspaceOptions`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub workspace_options: crate::options::WorkspaceOptions,
}

/// One plugin's isolated shada namespace: its name and full key→value map. The
/// values are opaque strings the plugin serialized (the Lua API JSON-encodes), so
/// core carries them blind.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginNamespace {
    pub namespace: String,
    pub entries: Vec<PluginEntry>,
}

/// One key/value pair inside a [`PluginNamespace`]. `value` is the plugin's own
/// serialized blob (JSON, from the `btv.shada.plugin` Lua API) — opaque to core.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginEntry {
    pub key: String,
    pub value: String,
}

/// One `btv.ui.input` history namespace and its ring, oldest entry first — the
/// persistent form of an [`Editor::prompt_history`] entry.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InputHistoryEntry {
    pub namespace: String,
    pub entries: Vec<String>,
}

/// A captured editor **session**: every tab's EXACT split layout (nesting + sizes) with
/// the open file + view at each leaf, plus which tab was focused, the global quickfix
/// stack, and each window's location list. Restored eagerly at boot
/// ([`Editor::restore_session`]). Window ids are deliberately NOT stored — they're
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
    /// File-backed buffers that were **loaded but not shown in any window** at capture —
    /// the hidden buffers you reach with `:bnext` / `:ls` (e.g. you `:edit` a second file,
    /// leaving the first hidden). Re-added to the buffer list (windowless) on restore so
    /// the whole working set survives, not just what happened to be on screen. Empty when
    /// every loaded buffer was visible. `#[serde(default)]` so an older store still loads.
    #[cfg_attr(feature = "serde", serde(default))]
    pub hidden_buffers: Vec<SessionHiddenBuffer>,
    /// The keyword of the layer that held focus at capture: `"main"`, or a dock side
    /// (`"left"`/`"right"`/`"top"`/`"bottom"`). The layout itself records which *window*
    /// was active within each tab/dock, but not which *layer* the cursor sat in — so a
    /// session quit from the main area while a dock was open used to reopen with the cursor
    /// stranded in the dock (its plugin grabs focus as it re-adopts the dock). Restored last,
    /// after the layout + any persisted-view adoption settle, so focus lands where you left
    /// it. `#[serde(default)]` → an older store (or `""`) falls back to the main layer.
    #[cfg_attr(feature = "serde", serde(default))]
    pub focus_layer: String,
}

/// One **hidden** (loaded-but-windowless) file-backed buffer captured into the session: its
/// path and its saved view (cursor + scroll), re-opened into the buffer list on restore.
/// Only ordinary file buffers ride this — terminals, images, and plugin views are excluded,
/// as they are from the window-leaf capture.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionHiddenBuffer {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub top: usize,
}

/// One edge dock: which side, its reserved size in cells (the boot-time fallback), an
/// optional `size_pct` (the size as a percentage of the screen, when captured with
/// `relativedocks` so the dock scales), whether it was hidden (parked), and its
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
/// was the tab's focused window. A leaf is captured when it shows a file (`path` set) OR,
/// with `'workspacepersistunnamed'` on, a modified **unnamed** `[No Name]` buffer — then
/// `path` is empty and [`SessionWindow::unnamed_contents`] carries the buffer's text.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionWindow {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub top: usize,
    pub active: bool,
    /// For an **unnamed** (pathless) `[No Name]` buffer persisted with its modified
    /// contents: the buffer's editable lines. `None` for a file-backed window (its
    /// content lives on disk and is reread on restore). Restored as a fresh unnamed
    /// buffer, marked modified so it round-trips on the next exit.
    #[cfg_attr(feature = "serde", serde(default))]
    pub unnamed_contents: Option<Vec<String>>,
    /// For a leaf showing a **persisted plugin view** (`btv.view.create{ persist = }`): the
    /// `(namespace, id)` the plugin chose. `path` is empty and `unnamed_contents` is `None`
    /// — core stores only this opaque pair, never the view's content. On restore the slot
    /// is reserved as an empty placeholder window and the owning plugin (resolved by
    /// `namespace`) adopts it and rebuilds its content. `None` for an ordinary leaf.
    #[cfg_attr(feature = "serde", serde(default))]
    pub view_persist: Option<(String, String)>,
}

/// A plugin view whose slot a session restore reserved (a `view_persist` leaf): the owning
/// plugin `namespace`, the plugin-chosen `id`, and the reserved placeholder `win`. Accrued
/// on [`Editor::restore_session`] into [`Editor::pending_view_restores`], mirrored to Lua
/// as `btv._view_pending`, and drained by the `btv.view.on_restore` dispatch once plugins
/// load — the owning plugin recreates its view and adopts `win`. Any entry left unclaimed
/// is an orphan (its plugin is gone) and its slot collapses.
#[derive(Debug, Clone)]
pub struct PendingViewRestore {
    pub namespace: String,
    pub id: String,
    pub win: crate::WindowId,
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

/// One persisted file's **manual** code folds: the file and its folds, each an
/// inclusive 0-based `(start, end)` line range plus whether it was closed. Only
/// `foldmethod=manual` folds persist — computed sources (indent / expr / LSP)
/// regenerate themselves on open. Restored into the window that reopens the file,
/// vim's `:mkview`-style fold persistence carried in shada.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileFolds {
    pub path: PathBuf,
    /// `(start, end, closed)` per fold, outer-before-inner.
    pub folds: Vec<(usize, usize, bool)>,
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
    /// Merge restored ex / search / input-namespace history into the live rings (older
    /// entries ahead of what's there, dropping duplicates) and re-cap to `'history'`. The
    /// history-only counterpart of [`import_persist`](Self::import_persist), used by the
    /// server to fold in the **global** history store post-config (`persisthistory`
    /// including `global`) without disturbing the marks / registers the primary store
    /// seeded.
    pub fn merge_persisted_history(
        &mut self,
        ex: Vec<String>,
        search: Vec<String>,
        input: Vec<InputHistoryEntry>,
    ) {
        merge_history(&mut self.ex_history, ex);
        merge_history(&mut self.search_history, search);
        self.merge_input_history(input);
        self.cap_history();
    }

    /// Fold restored `btv.ui.input` namespace rings into the live [`Editor::prompt_history`]
    /// (per namespace, older entries ahead, dropping duplicates). Does **not** cap — the
    /// caller re-caps every ring via [`cap_history`](Self::cap_history) once, after all
    /// merges.
    fn merge_input_history(&mut self, input: Vec<InputHistoryEntry>) {
        for entry in input {
            let ring = self.prompt_history.entry(entry.namespace).or_default();
            merge_history(ring, entry.entries);
        }
    }

    /// The per-namespace `btv.ui.input` history as persistable entries, namespaces sorted
    /// so a flush is deterministic across runs (the live map's iteration order isn't).
    fn export_input_history(&self) -> Vec<InputHistoryEntry> {
        let mut out: Vec<InputHistoryEntry> = self
            .prompt_history
            .iter()
            .map(|(namespace, entries)| InputHistoryEntry {
                namespace: namespace.clone(),
                entries: entries.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.namespace.cmp(&b.namespace));
        out
    }

    /// The ex / search / input-namespace history snapshot, for a history-only flush to the
    /// global store.
    pub fn export_history(&self) -> (Vec<String>, Vec<String>, Vec<InputHistoryEntry>) {
        (
            self.ex_history.clone(),
            self.search_history.clone(),
            self.export_input_history(),
        )
    }

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
            input_history: self.export_input_history(),
            numbered_marks: self.export_numbered_marks(),
            file_changelists: self.export_changelists(),
            file_folds: self.export_folds(),
            jumplist: self.export_jumplist(),
            exit_cursor: self.export_exit_cursor(),
            // The session rides the store only for a namespaced workspace; the server
            // attaches it via export_session() when enabled, so export_persist leaves
            // it None (the global shada never carries layout).
            session: None,
            // Plugin data lives in the Lua runtime, not the editor model; the server
            // attaches it at flush (LuaRuntime::plugin_shada_export), so core leaves
            // it empty here.
            plugin_data: Vec::new(),
            // The workspace option overlay rides the store only for a workspace-scoped
            // session — the server attaches it at flush (like `session`), so the global
            // shada never carries per-workspace overrides. Empty here.
            workspace_options: crate::options::WorkspaceOptions::new(),
        }
    }

    /// Capture the current tab + EXACT split layout for a workspace session: each tab's
    /// nesting + proportional sizes, with the file path / cursor / scroll at every leaf
    /// and the focused window marked. Floating windows and unnamed (scratch) buffers are
    /// dropped (single-child splits collapse). Returns `None` when nothing is worth
    /// saving. Pure: reads live state, no I/O.
    /// The native `'relativesplits'` option (default on) stores split sizes as
    /// proportional percentages rather than absolute cells; `'relativedocks'` (default
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
            // Capture against the tab's OWN tree (live for the current tab, the stashed
            // tree for an inactive tab). The `self.window_*` accessors only see the
            // current layer's tree, so an inactive tab's window ids resolve to nothing
            // through them — capturing here drops the whole tab. Read the tree directly.
            let Some(tree) = self.tab_tree(tid) else {
                continue;
            };
            // The window whose view lives on `self.cursor`/`self.top` (rather than its
            // stashed `saved_*`) is the focused main window — only when *this* tab is the
            // current one and the Main layer holds focus (a focused dock parks the main
            // tree, so its windows carry their saved view).
            let live = (tid == current_tab && self.focused_layer == super::Layer::Main)
                .then_some(self.windows.current);
            let node = tree.layout_node();
            let active = Some(tree.current);
            // Main-area windows may persist a modified `[No Name]` buffer (when
            // `'workspacepersistunnamed'` is on); docks never do (they reopen empty).
            let allow_unnamed = self.options.workspace_persist_unnamed;
            let layout = match self.capture_layout(
                &node,
                tree,
                live,
                active,
                relative_splits,
                allow_unnamed,
            ) {
                Some(l) => l,
                None => continue, // no file-backed leaf in this tab
            };
            if tid == current_tab {
                active_tab = tabs.len();
            }
            tabs.push(SessionTab { layout });
        }
        let docks = self.export_docks();
        let hidden_buffers = self.export_hidden_buffers();
        if tabs.is_empty() && docks.is_empty() && hidden_buffers.is_empty() {
            return None;
        }
        let focus_layer = match self.focused_layer {
            super::Layer::Main => "main".to_string(),
            super::Layer::Dock(side) => side.keyword().to_string(),
        };
        Some(SessionState {
            tabs,
            active_tab,
            docks,
            hidden_buffers,
            focus_layer,
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
            let live = (self.focused_layer == Layer::Dock(side)).then_some(self.windows.current);
            // A dock persists a modified ordinary `[No Name]` buffer the same as a main
            // window (gated on `'workspacepersistunnamed'`): a user can park an editable
            // scratch in a dock and expects it to ride the session. A *plugin* dock still
            // reopens empty for its owner to repopulate — its view buffer is `read_only`,
            // which `capture_layout` excludes, so this only captures genuine user content.
            let allow_unnamed = self.options.workspace_persist_unnamed;
            let layout = self.layer_tree(Layer::Dock(side)).and_then(|t| {
                self.capture_layout(
                    &t.layout_node(),
                    t,
                    live,
                    Some(t.current),
                    relative_splits,
                    allow_unnamed,
                )
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

    /// The set of buffer ids shown in **any** window — every main tab's tree plus every
    /// edge dock's tree (leaves + floats). Used to tell a hidden buffer (loaded but in no
    /// window) from a visible one when capturing the session. Over-including a windowed
    /// buffer here is harmless (it just isn't captured as hidden); the invariant that
    /// matters is that a truly hidden buffer is never in this set (it is in no window).
    fn windowed_buffers(&self) -> std::collections::HashSet<crate::BufferId> {
        use super::{DockSide, Layer};
        let mut shown = std::collections::HashSet::new();
        let add = |shown: &mut std::collections::HashSet<crate::BufferId>,
                   tree: &super::windows::WindowTree| {
            for wid in tree.leaves() {
                if let Some(w) = tree.try_get(wid) {
                    shown.insert(w.buffer);
                }
            }
            for &wid in &tree.floats {
                if let Some(w) = tree.try_get(wid) {
                    shown.insert(w.buffer);
                }
            }
        };
        for tid in self.tab_ids() {
            if let Some(tree) = self.tab_tree(tid) {
                add(&mut shown, tree);
            }
        }
        for side in DockSide::ALL {
            if let Some(tree) = self.layer_tree(Layer::Dock(side)) {
                add(&mut shown, tree);
            }
        }
        shown
    }

    /// Capture every **hidden** (loaded, file-backed, but windowless) buffer — the working
    /// set you reach with `:bnext` / `:ls` beyond what's on screen. The eligibility rule is
    /// exactly `:ls`'s ([`Editor::is_listed_buffer`]): only listed *documents* ride the
    /// session — never the non-document surfaces `:ls` hides (plugin views, panels, doc
    /// floats), so the session never saves a buffer the user can't see in `:ls`. Restored by
    /// re-reading the path, so a buffer with no name (an unnamed scratch, a terminal) is not
    /// captured here — a *windowed* modified `[No Name]` rides the layout via
    /// `'workspacepersistunnamed'` instead. The saved view (cursor + scroll) rides along so
    /// `:b` lands where you left it.
    fn export_hidden_buffers(&self) -> Vec<SessionHiddenBuffer> {
        let shown = self.windowed_buffers();
        let mut out = Vec::new();
        for (&id, ob) in &self.buffers.map {
            if shown.contains(&id) || !self.is_listed_buffer(id) {
                continue;
            }
            let Some(path) = ob
                .buffer
                .path
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
            else {
                continue;
            };
            out.push(SessionHiddenBuffer {
                path: path.clone(),
                line: ob.saved_cursor.line,
                col: ob.saved_cursor.col,
                top: ob.saved_top,
            });
        }
        out
    }

    /// Project a window-model [`LayoutNode`] into a serialisable [`SessionLayout`],
    /// resolving each leaf window to its file + view. A leaf with no file is dropped —
    /// UNLESS `allow_unnamed` and it shows a modified ordinary `[No Name]` buffer, whose
    /// contents are then captured into the leaf. A split left with one child collapses to
    /// that child; `None` when nothing survives. With `relative_splits` the kept sizes are
    /// normalized to percentages (summing ~100) — the restore re-lays them out either way,
    /// since the window model treats split sizes as proportional weights.
    ///
    /// `tree` is the tab's own [`WindowTree`] (live or stashed) — leaves resolve their
    /// buffer + view against it, never through the `self.window_*` accessors, which only
    /// see the current layer's tree and so would drop every inactive tab's windows. `live`
    /// names the one window (if any) whose view is on `self.cursor`/`self.top` rather than
    /// its stashed `saved_*`; `active` is the tab's focused window.
    fn capture_layout(
        &self,
        node: &super::windows::LayoutNode,
        tree: &super::windows::WindowTree,
        live: Option<crate::WindowId>,
        active: Option<crate::WindowId>,
        relative_splits: bool,
        allow_unnamed: bool,
    ) -> Option<SessionLayout> {
        use super::windows::LayoutNode;
        match node {
            LayoutNode::Leaf(wid) => {
                let w = tree.try_get(*wid)?;
                // A plugin view that opted into persistence (`btv.view.create{ persist = }`)
                // is captured by its `(namespace, id)` regardless of the read-only refusal
                // below — that's the one sanctioned way a `read_only` buffer rides the
                // session. Independent of `'workspacepersistunnamed'`.
                let view_persist = self
                    .buffers
                    .get(w.buffer)
                    .buffer
                    .view_id()
                    .and_then(|vid| self.view_persist_of(vid));
                // A non-document SURFACE shown in this window — a panel (`[Messages]`,
                // `[Buffers]`, …), a doc float, or an *unpersisted* plugin view — is never
                // part of the saved layout. A panel buffer is *named* (`[Messages]`), so
                // without this it would be captured as a "file" leaf and restored as a split
                // holding an empty buffer named for the panel. Drop the leaf (its split
                // collapses); only listed documents and persisted views ride the session.
                if view_persist.is_none() && !self.is_listed_buffer(w.buffer) {
                    return None;
                }
                let path = self.buffer_name(w.buffer).unwrap_or_default();
                // A pathless leaf is dropped unless it's a persisted view (handled above) OR
                // a modified ordinary `[No Name]` buffer we're allowed to persist
                // (`'workspacepersistunnamed'`): then we capture its lines instead of a path.
                // Other non-ordinary buffers (terminals, images, unpersisted views —
                // `read_only`) are never captured this way.
                let unnamed_contents = if path.is_empty() && view_persist.is_none() {
                    let buf = &self.buffers.get(w.buffer).buffer;
                    if !allow_unnamed || buf.read_only() || !buf.modified {
                        return None;
                    }
                    Some(buf.lines())
                } else {
                    None
                };
                let (line, col, top) = if live == Some(*wid) {
                    (self.cursor.line, self.cursor.col, self.top)
                } else {
                    (w.saved_cursor.line, w.saved_cursor.col, w.saved_top)
                };
                Some(SessionLayout::Leaf(SessionWindow {
                    path: PathBuf::from(path),
                    line,
                    col,
                    top,
                    active: active == Some(*wid),
                    unnamed_contents,
                    view_persist,
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
                    if let Some(sl) = self.capture_layout(
                        child,
                        tree,
                        live,
                        active,
                        relative_splits,
                        allow_unnamed,
                    ) {
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
        use crate::WindowId;
        use std::collections::BTreeMap;
        if session.tabs.is_empty() && session.docks.is_empty() && session.hidden_buffers.is_empty()
        {
            return;
        }
        // Every window the restore mints is a NEW window, so it inherits the current
        // window's window-local options — exactly as a `:split` / `:tabnew` does
        // ([`Editor::split`]). The restore runs after the config is sourced, so this
        // template is the startup window carrying the user's `vim.opt` window settings
        // (`scrolloff`, `signcolumn`, `number`, …); without it a restored session came
        // back with every window at the built-in defaults and the config's window
        // options silently lost.
        let template = self.windows.cur().options.clone();
        // Re-add the hidden (windowless) buffers to the buffer list FIRST, before the windows
        // are built — so they exist when `:bnext`/`:ls` enumerate, and a windowed leaf that
        // happens to name the same file finds the already-loaded buffer (no duplicate).
        self.restore_hidden_buffers(&session.hidden_buffers);
        // Restore the edge docks FIRST, so the main area is already dock-reduced before any
        // tab's split tree is laid out. Restoring tabs first would lay every split out at
        // FULL width and only rescale it once a dock later shrinks the main area — and that
        // second, lossy rescale drifts a balanced split off its saved proportions. With the
        // docks in place up front, each tab is laid out exactly once, at its real width.
        self.restore_docks(&session.docks, &template);
        // `restore_docks` leaves a dock as the focused layer; tab ops (`new_tab` /
        // `install_restored_tree`) must run on the main tree, so cross back first.
        self.ensure_main_layer();
        let mut built_any = false;
        for tab in &session.tabs {
            let mut windows: BTreeMap<WindowId, super::windows::Window> = BTreeMap::new();
            let mut active: Option<WindowId> = None;
            let root = match self.build_layout(&tab.layout, &template, &mut windows, &mut active) {
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
                self.new_tab(buf, template.clone());
            }
            built_any = true;
            self.install_restored_tree(tree);
        }
        // Focus the saved active tab — this leaves the editor focused on the main layer,
        // with the docks parked behind it.
        let tab_ids = self.tab_ids();
        if let Some(tid) = tab_ids.get(session.active_tab.min(tab_ids.len().saturating_sub(1))) {
            self.set_current_tabpage(*tid);
        }
        // Stash the captured focus layer to apply once the layout (and any persisted-view
        // adoption that re-mounts a dock) has settled — see [`finalize_session_focus`]. We
        // can't focus a dock here: it may still hold an unadopted placeholder, and we just
        // had to land on the main layer for the tab build above.
        if !session.focus_layer.is_empty() {
            self.pending_session_focus = Some(session.focus_layer);
        }
    }

    /// Whether a restored session's focus layer is still being held — the boot-restore
    /// window is open and no user input has taken over yet (see [`finalize_session_focus`]).
    pub fn session_focus_pending(&self) -> bool {
        self.pending_session_focus.is_some()
    }

    /// Release the restored-focus hold: the user has acted (a key / mouse), so from here
    /// their own focus choices win. Called on the first real user input.
    pub fn clear_session_focus_hold(&mut self) {
        self.pending_session_focus = None;
    }

    /// Re-assert the focus layer a session restore captured in
    /// [`Editor::pending_session_focus`] — so a session reopens with the cursor in the layer
    /// you left it (the main area, or a dock), undoing any focus a sidebar plugin grabbed
    /// while (re)building its dock. **Peeks**, never clears: a file-tree's async mount can
    /// focus its dock many ticks into startup, well past the one VimEnter point, so the hold
    /// re-applies on every settle until the first user input releases it
    /// ([`clear_session_focus_hold`]). A dock that didn't come back (orphaned / closed) falls
    /// back to the main layer. A no-op once focus already sits where the restore wanted it
    /// (or nothing was stashed); returns whether it moved focus this call.
    pub fn finalize_session_focus(&mut self) -> bool {
        let Some(keyword) = self.pending_session_focus.as_deref() else {
            return false;
        };
        let target = if keyword == "main" {
            super::Layer::Main
        } else {
            match super::DockSide::from_keyword(keyword) {
                // A captured dock that is no longer open (orphaned view, closed) leaves
                // focus on the main layer rather than erroring — graceful, like a vanished
                // file's window collapsing.
                Some(side) if self.layer_is_open(super::Layer::Dock(side)) => {
                    super::Layer::Dock(side)
                }
                _ => super::Layer::Main,
            }
        };
        if self.focused_layer == target {
            return false;
        }
        self.switch_layer(target);
        true
    }

    /// The pending persisted-view slots as `(namespace, id, win)` for the Lua mirror
    /// (`btv._view_pending`, read by `btv.view.pending_restores()` and the `on_restore`
    /// dispatch). Empty outside the boot-restore window.
    pub fn view_pending_restores(&self) -> Vec<(String, String, u64)> {
        self.pending_view_restores
            .iter()
            .map(|p| (p.namespace.clone(), p.id.clone(), p.win.0))
            .collect()
    }

    /// Reopen each saved [`SessionDock`] at its side + size, rebuilding any file-backed
    /// content (a plugin dock reopens empty for its owner to repopulate) and re-hiding a
    /// dock that was parked. `template` is the window-option seed every rebuilt window
    /// inherits (see [`Editor::restore_session`]).
    fn restore_docks(&mut self, docks: &[SessionDock], template: &crate::options::WindowOptions) {
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
                let root = self.build_layout(layout, template, &mut windows, &mut active)?;
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

    /// Re-open each captured **hidden** buffer into the buffer list without placing it in a
    /// window (the find-or-load `open_buffer` adds it and returns without switching). Its
    /// saved view (cursor + scroll) is restored onto the loaded buffer so a later `:b` lands
    /// where it was. A file that no longer opens is skipped. Runs before the window layout is
    /// rebuilt, so a windowed leaf naming the same file reuses the buffer loaded here.
    fn restore_hidden_buffers(&mut self, hidden: &[SessionHiddenBuffer]) {
        for h in hidden {
            if h.path.as_os_str().is_empty() {
                continue;
            }
            if let Some(id) = self.open_buffer_for_restore(&h.path) {
                let ob = self.buffers.get_mut(id);
                ob.saved_cursor = Cursor {
                    line: h.line,
                    col: h.col,
                };
                ob.saved_top = h.top;
            }
        }
    }

    /// Resolve a leaf's buffer on restore: reopen its file (`path` set), or — for a
    /// persisted **unnamed** `[No Name]` buffer (`unnamed_contents`) — create a fresh
    /// buffer, load the saved lines, and mark it modified so it round-trips on the next
    /// exit (its content is unsaved, exactly as it was). `None` when a file no longer
    /// opens or a pathless leaf carries no contents.
    fn build_leaf_buffer(&mut self, w: &SessionWindow) -> Option<crate::BufferId> {
        if !w.path.as_os_str().is_empty() {
            return self.open_buffer_for_restore(&w.path);
        }
        let lines = w.unnamed_contents.as_ref()?;
        let id = self.create_buffer();
        self.load_str_into(id, None, &lines.join("\n"));
        // `load_str_into` marks the replica clean; a restored `[No Name]` is unsaved by
        // definition, so flag it modified (keeps `:qa` honest and round-trips capture).
        self.buffers.get_mut(id).buffer.modified = true;
        Some(id)
    }

    /// Recursively realise a [`SessionLayout`] into a window map + a [`LayoutNode`]
    /// skeleton: open each leaf's file (minting a fresh window id), drop leaves whose
    /// file is gone, and collapse a split left with one child. `None` if nothing opens.
    /// Every minted window starts from `template` — the current window's options — so a
    /// restored layout carries the config's window-local settings (see
    /// [`Editor::restore_session`]).
    fn build_layout(
        &mut self,
        layout: &SessionLayout,
        template: &crate::options::WindowOptions,
        windows: &mut std::collections::BTreeMap<crate::WindowId, super::windows::Window>,
        active: &mut Option<crate::WindowId>,
    ) -> Option<super::windows::LayoutNode> {
        use super::windows::{LayoutNode, Window, WindowTree};
        match layout {
            SessionLayout::Leaf(w) => {
                // A persisted plugin view: reserve the slot with an empty placeholder buffer
                // and record a pending claim keyed by `(namespace, id)`. The owning plugin
                // adopts the reserved window after it loads (`btv.view.on_restore`); an
                // unclaimed slot collapses at the end of the restore tick. We never recreate
                // the view here — its content, callbacks, and keymaps all live in the plugin.
                if let Some((namespace, vid)) = &w.view_persist {
                    let buf = self.create_buffer();
                    let id = self.alloc_window_id();
                    let cursor = Cursor {
                        line: w.line,
                        col: w.col,
                    };
                    let win = Window {
                        options: template.clone(),
                        ..WindowTree::tiled_window(buf, cursor, w.top, 0)
                    };
                    windows.insert(id, win);
                    if w.active {
                        *active = Some(id);
                    }
                    self.pending_view_restores.push(PendingViewRestore {
                        namespace: namespace.clone(),
                        id: vid.clone(),
                        win: id,
                    });
                    return Some(LayoutNode::Leaf(id));
                }
                let buf = self.build_leaf_buffer(w)?;
                let id = self.alloc_window_id();
                let cursor = Cursor {
                    line: w.line,
                    col: w.col,
                };
                let win = Window {
                    options: template.clone(),
                    ..WindowTree::tiled_window(buf, cursor, w.top, 0)
                };
                windows.insert(id, win);
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
                    if let Some(node) = self.build_layout(child, template, windows, active) {
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
        // Manual folds: keyed by normalized path, seeded into the window that
        // reopens the file (drained by `seed_pending_folds`).
        for entry in state.file_folds {
            self.pending_folds
                .entry(super::normalize_path(&entry.path))
                .or_insert(entry.folds);
        }
        let cur = self.cur_buffer();
        self.seed_pending_file_marks(cur);
        self.seed_pending_folds();
        // History restored from disk is older than anything typed this session;
        // merge it *ahead* of the (empty, at startup) live history, dropping older
        // duplicates so a repeated entry keeps its newest position.
        merge_history(&mut self.search_history, state.search_history);
        merge_history(&mut self.ex_history, state.ex_history);
        self.merge_input_history(state.input_history);
        // A restored ring can exceed the live `'history'` cap (a smaller value than the
        // store's persistence ceiling); trim to it.
        self.cap_history();
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
        // Seed the per-workspace option overlay (`btv.wso`) and apply it. Additive: a live
        // override set this session is kept (unless `replace`); a restored one fills an
        // unset key. The overlay wins over the global base, so the recompute makes the
        // effective options reflect the workspace overrides regardless of load order.
        if !state.workspace_options.is_empty() {
            for (name, value) in state.workspace_options {
                if replace || !self.workspace_options.contains_key(&name) {
                    self.workspace_options.insert(name, value);
                }
            }
            self.recompute_effective_options();
        }
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
