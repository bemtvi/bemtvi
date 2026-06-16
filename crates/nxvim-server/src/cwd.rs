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
use std::path::{Path, PathBuf};

use nxvim_core::{TabId, WindowId};

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
#[derive(Default)]
struct Entry {
    dir: PathBuf,
    prev: Option<PathBuf>,
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
