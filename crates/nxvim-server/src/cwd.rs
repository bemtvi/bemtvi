//! The working-directory model behind `:cd` / `:tcd` / `:lcd`.
//!
//! vim layers the current directory across three scopes: a **window-local** dir
//! (`:lcd`) overrides a **tab-local** dir (`:tcd`), which overrides the **global**
//! dir (`:cd`). The *process* cwd is always the effective dir of the current
//! window — `vim.fn.getcwd` reads it and every relative path resolves against it —
//! re-applied whenever focus moves between windows / tabs (the lifecycle's
//! `fix_current_dir`). This struct owns the per-scope state; the process cwd is
//! mutated by the `excmd` handlers and that switch hook.
//!
//! Stored dirs are always the OS-canonical absolute paths (the handlers store what
//! `current_dir()` reports after the `chdir`), so [`DirState::effective`] can be
//! compared against the live process cwd without re-canonicalizing.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use nxvim_core::{TabId, WindowId};

/// A deferred (daemon) `:cd` awaiting its `fs_chdir` reply: which scope/window/tab it
/// targets, and the [`CdUndo`] for the optimistic move it already applied (`None` for a
/// `""`/`~` target, whose home only the daemon can resolve — those don't move
/// optimistically and so install only on the ack). Held in `EditHost::pending_chdirs`,
/// keyed by the token threaded through [`HostEffects::fs_chdir`](crate::HostEffects::fs_chdir),
/// and consumed by `apply_chdir` when the canonical path (or `E344`) lands. See
/// `docs/plans/2026-06-23-remote-cwd.md`.
pub(crate) struct PendingChdir {
    pub scope: CdScope,
    pub win: WindowId,
    pub tab: TabId,
    pub undo: Option<CdUndo>,
}

/// The reply leg of a deferred `:cd`: the `pending_chdirs` token it was issued under, plus
/// the daemon's result — the canonical directory (`Ok`) or the loud `E344`/transport error
/// (`Err`). Delivered inbound on the run loop's chdir arm to `EditHost::apply_chdir`.
pub(crate) struct ChdirDone {
    pub token: u64,
    pub result: io::Result<String>,
}

/// Which scope a `:cd`-family command targets (vim's `CdScope`, in override order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CdScope {
    /// `:lcd` — the current window only.
    Window,
    /// `:tcd` — the current tab page.
    Tabpage,
    /// `:cd` — the global directory new windows / tabs inherit.
    Global,
}

impl CdScope {
    /// The `DirChanged` autocmd pattern (and `v:event.scope`) for this scope.
    pub(crate) fn pattern(self) -> &'static str {
        match self {
            CdScope::Window => "window",
            CdScope::Tabpage => "tabpage",
            CdScope::Global => "global",
        }
    }
}

/// A scope's current dir plus the previous one (for the `:cd -` toggle).
#[derive(Default, Clone)]
struct Entry {
    dir: PathBuf,
    prev: Option<PathBuf>,
}

/// An undo token for an *optimistic* `:cd` (the daemon path): the snapshot of the three
/// scope slots a [`DirState::set`] could touch, taken **before** the optimistic move, so a
/// later daemon rejection (`E344`) or a canonical-path correction can reverse it exactly.
/// `optimistic` is the dir the move installed at `scope` — [`DirState::rollback_optimistic`]
/// only reverses when that dir is still in place, so a *newer* `:cd` that already
/// superseded this one is never clobbered. See `docs/plans/2026-06-23-remote-cwd.md`.
pub(crate) struct CdUndo {
    scope: CdScope,
    win: WindowId,
    tab: TabId,
    optimistic: PathBuf,
    win_slot: Option<Entry>,
    tab_slot: Option<Entry>,
    global_slot: Entry,
}

/// The three-scope directory state. The current window's effective dir is mirrored
/// into the process cwd by the callers; this only owns the scope bookkeeping.
pub(crate) struct DirState {
    global: Entry,
    win: HashMap<WindowId, Entry>,
    tab: HashMap<TabId, Entry>,
}

impl DirState {
    /// Seed the global dir from the process cwd at startup; no local dirs yet.
    pub(crate) fn new(start: PathBuf) -> Self {
        DirState {
            global: Entry {
                dir: start,
                prev: None,
            },
            win: HashMap::new(),
            tab: HashMap::new(),
        }
    }

    /// The effective dir for `(win, tab)` and the scope it came from: a
    /// window-local dir wins, then a tab-local dir, then the global dir.
    pub(crate) fn effective(&self, win: WindowId, tab: TabId) -> (CdScope, &Path) {
        if let Some(e) = self.win.get(&win) {
            (CdScope::Window, &e.dir)
        } else if let Some(e) = self.tab.get(&tab) {
            (CdScope::Tabpage, &e.dir)
        } else {
            (CdScope::Global, &self.global.dir)
        }
    }

    /// The previous dir for `scope` (the `-` target), or `None` if there is none.
    pub(crate) fn prev(&self, scope: CdScope, win: WindowId, tab: TabId) -> Option<&Path> {
        match scope {
            CdScope::Global => self.global.prev.as_deref(),
            CdScope::Tabpage => self.tab.get(&tab).and_then(|e| e.prev.as_deref()),
            CdScope::Window => self.win.get(&win).and_then(|e| e.prev.as_deref()),
        }
    }

    /// Apply a `:cd` / `:tcd` / `:lcd` to `new_dir` for `(win, tab)`, following
    /// vim's `post_chdir` clearing rules: any command clears the current
    /// **window**-local dir, and `:cd` additionally clears the current **tab**-local
    /// dir, before the new dir is installed at its scope. The replaced dir (if any)
    /// becomes that scope's `prev` for a later `-`.
    pub(crate) fn set(&mut self, scope: CdScope, win: WindowId, tab: TabId, new_dir: PathBuf) {
        match scope {
            CdScope::Global => {
                self.win.remove(&win);
                self.tab.remove(&tab);
                self.global.prev = Some(std::mem::replace(&mut self.global.dir, new_dir));
            }
            CdScope::Tabpage => {
                self.win.remove(&win);
                set_local(self.tab.entry(tab).or_default(), new_dir);
            }
            CdScope::Window => {
                set_local(self.win.entry(win).or_default(), new_dir);
            }
        }
    }

    /// Apply a `:cd` **optimistically** (the daemon path), returning a [`CdUndo`] that can
    /// reverse it once the daemon confirms or rejects. Same effect as [`Self::set`], but it
    /// first snapshots the three slots `set` may touch so [`Self::rollback_optimistic`] can
    /// restore them verbatim — including the `prev` pointers and the window/tab-local dirs a
    /// global `:cd` clears. The move takes effect immediately so an `:e` / `getcwd` in the
    /// same tick sees the new dir; the announcing `DirChanged` is deferred to the ack.
    pub(crate) fn set_optimistic(
        &mut self,
        scope: CdScope,
        win: WindowId,
        tab: TabId,
        new_dir: PathBuf,
    ) -> CdUndo {
        let undo = CdUndo {
            scope,
            win,
            tab,
            optimistic: new_dir.clone(),
            win_slot: self.win.get(&win).cloned(),
            tab_slot: self.tab.get(&tab).cloned(),
            global_slot: self.global.clone(),
        };
        self.set(scope, win, tab, new_dir);
        undo
    }

    /// The dir installed *at* `scope` (its own entry, not the effective override), or
    /// `None` if that scope has no local dir — used to tell whether an optimistic `:cd` is
    /// still in place or was superseded by a later one.
    fn dir_at(&self, scope: CdScope, win: WindowId, tab: TabId) -> Option<&Path> {
        match scope {
            CdScope::Global => Some(&self.global.dir),
            CdScope::Tabpage => self.tab.get(&tab).map(|e| e.dir.as_path()),
            CdScope::Window => self.win.get(&win).map(|e| e.dir.as_path()),
        }
    }

    /// Reverse an optimistic `:cd` ([`Self::set_optimistic`]) — restoring the snapshotted
    /// slots — **iff** its dir is still installed at the scope. Returns `true` when it
    /// rolled back, `false` when a later `:cd` already superseded this one (in which case
    /// the newer state, and its own announce, are left untouched). The caller uses the
    /// result to decide whether to finalize (ack-ok) or stop (superseded).
    pub(crate) fn rollback_optimistic(&mut self, undo: CdUndo) -> bool {
        if self.dir_at(undo.scope, undo.win, undo.tab) != Some(undo.optimistic.as_path()) {
            return false; // a newer :cd won — leave it
        }
        match undo.win_slot {
            Some(e) => {
                self.win.insert(undo.win, e);
            }
            None => {
                self.win.remove(&undo.win);
            }
        }
        match undo.tab_slot {
            Some(e) => {
                self.tab.insert(undo.tab, e);
            }
            None => {
                self.tab.remove(&undo.tab);
            }
        }
        self.global = undo.global_slot;
        true
    }

    /// Drop a closed window's local dir (called from the lifecycle window-close
    /// sweep so a reused window id can't inherit a stale local dir).
    pub(crate) fn forget_window(&mut self, win: WindowId) {
        self.win.remove(&win);
    }

    /// Drop a closed tab's local dir (the tab-close analogue of [`forget_window`]).
    pub(crate) fn forget_tab(&mut self, tab: TabId) {
        self.tab.remove(&tab);
    }
}

/// Install `new_dir` as a window/tab-local entry, recording the displaced dir as
/// its `prev` — unless this is the entry's first set (the default `dir` is empty),
/// where there is no previous local dir to return to.
fn set_local(entry: &mut Entry, new_dir: PathBuf) {
    let old = std::mem::replace(&mut entry.dir, new_dir);
    if !old.as_os_str().is_empty() {
        entry.prev = Some(old);
    }
}
