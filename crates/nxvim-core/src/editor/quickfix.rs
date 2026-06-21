//! The quickfix / location list model and the `errorformat` engine.
//!
//! This is a faithful port of vim's `quickfix.c` error-parsing core: an
//! `'errorformat'` string is split into comma-separated *parts*, each part is
//! converted into a vim regex by [`efm_part_to_regpat`] (the port of vim's
//! `efm_to_regpat` / `efmpat_to_regpat` / `scanf_fmt_to_regpat`), and each output
//! line is matched against the parts in turn, pulling fields out of the regex
//! submatches (the port of `qf_parse_line` / `qf_parse_match` / `qf_parse_fmt_*`).
//! The multi-line prefixes (`%A %C %Z %E %W %I %N`), the exclude/append flags
//! (`%-` / `%+`), the `%>` continuation, and the `%D` / `%X` directory stack are
//! all honored.
//!
//! The data types ([`QfEntry`], [`QfList`]) are plain and always compiled; the
//! engine itself rides on [`nxvim_regex`] (vim's vendored `regexp.c`) and so lives
//! behind the `vim-regex` feature — the same engine `:s` / `/` use under
//! `regexsyntax=vim`. A build without it (a pure-Rust core) keeps the list types
//! but fails loud on any parse, never silently dropping lines.

use super::*;
use crate::buffer::Buffer;
use crate::WindowOptions;
use std::path::{Path, PathBuf};

/// How a populate request combines with the existing list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QfAction {
    /// Replace the list with the new items (vim's `' '` action / a fresh `:cexpr`).
    New,
    /// Append the new items to the current list (vim's `'a'`).
    Add,
    /// Replace the current list's items in place (vim's `'r'`). At the
    /// single-[`QfList`] level this swaps items like [`QfAction::New`]; the two
    /// diverge at the [`QfStack`] level, where `New` pushes a *new* list and
    /// `Replace` mutates the current one.
    Replace,
}

/// One parsed quickfix/location entry — vim's `qfline_T`, minus the
/// list-threading pointers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QfEntry {
    /// The error's file, resolved against the `%D`/`%X` directory stack. `None`
    /// for a non-error line (plain output text) or an entry addressed only by
    /// buffer number.
    pub filename: Option<String>,
    /// Buffer number from `%b` (`0` if none).
    pub bufnr: i32,
    /// Module name accumulated from `%o` (empty if none).
    pub module: String,
    /// 1-based line number (`0` = none / not jumpable).
    pub lnum: usize,
    /// 1-based end line number from `%e` (`0` = none).
    pub end_lnum: usize,
    /// 1-based column (`0` = none). Byte column unless [`QfEntry::vcol`].
    pub col: usize,
    /// 1-based end column from `%k` (`0` = none).
    pub end_col: usize,
    /// The column is a screen (virtual) column — set by `%v` / `%p`.
    pub vcol: bool,
    /// Error number from `%n` (`-1` = none).
    pub nr: i32,
    /// Search pattern from `%s` (empty if none), already wrapped as `^\V…\$`.
    pub pattern: String,
    /// The message text.
    pub text: String,
    /// Error type char: `'E'`/`'W'`/`'I'`/`'N'` (or a `%t` value), `0` if none.
    pub typ: u8,
    /// A real, jumpable error (vs. a copied non-matching output line).
    pub valid: bool,
}

/// A quickfix or location list: the entries plus a title and the current index.
#[derive(Debug, Clone, Default)]
pub struct QfList {
    /// The parsed entries, in output order.
    pub items: Vec<QfEntry>,
    /// The list title (`:copen` header / `getqflist({title})`). Empty if unset.
    pub title: String,
    /// The 1-based index of the "current" entry for `:cc`/`:cnext` (`0` = none).
    /// Tracked now so Phase 2 navigation has a home; unused until then.
    pub idx: usize,
}

impl QfList {
    /// Apply `items` under `action`, updating the title when one is given.
    fn apply(&mut self, items: Vec<QfEntry>, action: QfAction, title: Option<String>) {
        match action {
            QfAction::Add => self.items.extend(items),
            // At this single-list level `New` and `Replace` both swap the whole item
            // vector; the push-new-vs-mutate-current divergence lives in [`QfStack`].
            QfAction::New | QfAction::Replace => self.items = items,
        }
        if let Some(title) = title {
            self.title = title;
        }
        // A fresh/replaced list resets the cursor to the first entry; an append
        // leaves it. (`0` when the list is empty.)
        if action != QfAction::Add {
            self.idx = usize::from(!self.items.is_empty());
        }
    }
}

/// The most lists vim (and nxvim) keep in a quickfix/location-list stack; older
/// lists past this are dropped as new ones are pushed. (vim's `LISTCOUNT`.)
pub const QF_MAXLISTS: usize = 10;

/// A quickfix or location-list **stack**: the history of up to [`QF_MAXLISTS`]
/// lists `:colder`/`:cnewer` (`:lolder`/`:lnewer`) walk, with `cur` pointing at the
/// "current" one every other command reads and writes. vim keeps this so a fresh
/// `:make`/`:grep`/`:vimgrep`/`:cexpr` (action `' '`) pushes a new list without
/// losing the previous results.
#[derive(Debug, Clone, Default)]
pub struct QfStack {
    /// The lists, oldest first; `lists.last()` is the newest.
    pub lists: Vec<QfList>,
    /// 0-based index of the current list (only meaningful when `!lists.is_empty()`).
    pub cur: usize,
}

impl QfStack {
    /// The current list, if the stack is non-empty.
    pub fn current(&self) -> Option<&QfList> {
        self.lists.get(self.cur)
    }

    /// The current list mutably, if the stack is non-empty.
    pub fn current_mut(&mut self) -> Option<&mut QfList> {
        self.lists.get_mut(self.cur)
    }

    /// Apply `items` under `action`, mirroring vim's stack semantics:
    /// - `New` (`' '`): push a brand-new current list. Any lists *newer* than the
    ///   current one (you walked back with `:colder` then produced fresh results)
    ///   are discarded first, and the oldest is dropped once the stack would exceed
    ///   [`QF_MAXLISTS`].
    /// - `Add` (`'a'`) / `Replace` (`'r'`): modify the current list in place,
    ///   creating a first list if the stack is empty.
    fn apply(&mut self, items: Vec<QfEntry>, action: QfAction, title: Option<String>) {
        match action {
            QfAction::New => {
                if !self.lists.is_empty() {
                    self.lists.truncate(self.cur + 1);
                }
                let mut list = QfList::default();
                list.apply(items, QfAction::New, title);
                self.lists.push(list);
                if self.lists.len() > QF_MAXLISTS {
                    self.lists.remove(0);
                }
                self.cur = self.lists.len() - 1;
            }
            QfAction::Add | QfAction::Replace => {
                if self.lists.is_empty() {
                    let mut list = QfList::default();
                    list.apply(items, action, title);
                    self.lists.push(list);
                    self.cur = 0;
                } else {
                    self.lists[self.cur].apply(items, action, title);
                }
            }
        }
    }

    /// `:colder`/`:lolder` — step `count` lists toward the oldest. Returns whether
    /// the pointer actually moved (`false` already at the bottom → vim's `E380`).
    fn older(&mut self, count: usize) -> bool {
        let target = self.cur.saturating_sub(count.max(1));
        let moved = target != self.cur;
        self.cur = target;
        moved
    }

    /// `:cnewer`/`:lnewer` — step `count` lists toward the newest. Returns whether
    /// the pointer moved (`false` already at the top → vim's `E381`).
    fn newer(&mut self, count: usize) -> bool {
        if self.lists.is_empty() {
            return false;
        }
        let target = (self.cur + count.max(1)).min(self.lists.len() - 1);
        let moved = target != self.cur;
        self.cur = target;
        moved
    }
}

/// Which list a quickfix command targets: the single global quickfix list, or the
/// per-window **location list** owned by a specific window. Every `:c*`/`:l*` pair
/// shares one implementation parameterized by this — `:copen` is
/// `ex_qf_open(Quickfix, …)`, `:lopen` is `ex_qf_open(Location(win), …)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QfWhich {
    /// The global quickfix list (`:copen`, `:make`, `setqflist`, …).
    Quickfix,
    /// The location list owned by the given window (`:lopen`, `:lgrep`,
    /// `setloclist`, …).
    Location(WindowId),
}

impl QfWhich {
    /// vim's user-facing name for this list, used in `E776`/`E42`-style messages.
    fn label(self) -> &'static str {
        match self {
            QfWhich::Quickfix => "quickfix",
            QfWhich::Location(_) => "location",
        }
    }
}

/// A never-populated list returned by [`Editor::qf_list`] when the quickfix stack
/// is empty, so callers (the mirror push) can read `.items`/`.title` uniformly.
fn empty_qflist() -> &'static QfList {
    static EMPTY: std::sync::OnceLock<QfList> = std::sync::OnceLock::new();
    EMPTY.get_or_init(QfList::default)
}

impl Editor {
    // ---- list-stack accessors (the quickfix / per-window location list) ----

    /// Read-only access to a list stack. The quickfix stack always exists; a
    /// location-list stack exists only once its owner window has been given one
    /// (`None` otherwise, and `None` for a stale window id).
    fn qf_stack(&self, which: QfWhich) -> Option<&QfStack> {
        match which {
            QfWhich::Quickfix => Some(&self.qf),
            QfWhich::Location(w) => self.window(w).and_then(|win| win.loclist.as_ref()),
        }
    }

    /// The current list of `which`, or a shared empty list when the stack is
    /// absent/empty — so readers (rendering, the mirror push) need no `Option`.
    fn qf_cur(&self, which: QfWhich) -> &QfList {
        self.qf_stack(which)
            .and_then(QfStack::current)
            .unwrap_or_else(|| empty_qflist())
    }

    /// The current list of `which`, mutably, if the stack exists and is non-empty.
    fn qf_cur_mut(&mut self, which: QfWhich) -> Option<&mut QfList> {
        match which {
            QfWhich::Quickfix => self.qf.current_mut(),
            QfWhich::Location(w) => self
                .window_mut(w)
                .and_then(|win| win.loclist.as_mut())
                .and_then(QfStack::current_mut),
        }
    }

    /// Mutable list stack, **creating** an empty location-list stack on the owner
    /// window when needed (so `setloclist` / `:lvimgrep` on a fresh window has
    /// somewhere to write). `None` only for a stale location-window id.
    fn qf_stack_ensure(&mut self, which: QfWhich) -> Option<&mut QfStack> {
        match which {
            QfWhich::Quickfix => Some(&mut self.qf),
            QfWhich::Location(w) => self
                .window_mut(w)
                .map(|win| win.loclist.get_or_insert_with(QfStack::default)),
        }
    }

    /// The current quickfix list (read-only) — the projection source for the
    /// `nx._qflist` Lua mirror.
    pub fn qf_list(&self) -> &QfList {
        self.qf_cur(QfWhich::Quickfix)
    }

    /// The current location list of `win`, if it has one — the per-window mirror
    /// source for `getloclist()`.
    pub fn loclist(&self, win: WindowId) -> Option<&QfList> {
        self.qf_stack(QfWhich::Location(win))
            .and_then(QfStack::current)
    }

    /// The location-list context the `:l*` commands act on from the focused window:
    /// if focus is in a location-list *display* window, that display's owner (so
    /// `:lnext` from inside the loclist window steps the loclist it shows);
    /// otherwise the focused window itself.
    pub(crate) fn loclist_which(&self) -> QfWhich {
        match self.qf_context_of_buffer(self.current_buffer_id()) {
            Some(w @ QfWhich::Location(_)) => w,
            _ => QfWhich::Location(self.current_window_id()),
        }
    }

    // ---- populating a list ----

    /// Set a list from already-structured `items` (vim's `setqflist(list)` /
    /// `setloclist(win, list)` non-parsing form).
    pub fn qf_set_items(
        &mut self,
        which: QfWhich,
        items: Vec<QfEntry>,
        action: QfAction,
        title: Option<String>,
    ) {
        if let Some(stack) = self.qf_stack_ensure(which) {
            stack.apply(items, action, title);
        }
        self.qf_refresh_window(which);
    }

    /// Open the current window's **location list** from `entries` and show it
    /// (`:lopen`) — the navigable successor to the retired panel's
    /// `open_panel` + `set_panel_targets` for LSP reference / diagnostic lists.
    /// Each entry is `(path, line, col, text)` with **0-based** line/col (the form the
    /// old panel targets used); the loclist stores vim's 1-based columns. The list is
    /// pushed as a *new* list on the window's stack (recover a prior `:lgrep` with
    /// `:lolder`), and `<CR>` on a row jumps via the buffer-local `qf` map.
    pub fn open_location_list(
        &mut self,
        entries: Vec<(PathBuf, usize, usize, String)>,
        title: &str,
    ) {
        let which = QfWhich::Location(self.windows.current);
        let items = entries
            .into_iter()
            .map(|(path, line, col, text)| QfEntry {
                filename: Some(path.display().to_string()),
                lnum: line + 1,
                col: col + 1,
                text,
                valid: true,
                ..Default::default()
            })
            .collect();
        self.qf_set_items(which, items, QfAction::New, Some(title.to_string()));
        self.ex_qf_open(which, "");
    }

    /// Parse `lines` against `efm` and set the list (vim's
    /// `setqflist([], a, {lines, efm})` and the `:cexpr`/`:cfile` family). Returns
    /// the number of entries added, or an `E37x` error string for an invalid
    /// `'errorformat'`. Behind `vim-regex`; without it, parsing fails loud.
    #[cfg(feature = "vim-regex")]
    pub fn qf_set_from_lines(
        &mut self,
        which: QfWhich,
        lines: &[String],
        efm: &str,
        action: QfAction,
        title: Option<String>,
    ) -> Result<usize, String> {
        let format = Errorformat::compile(efm)?;
        let items = format.parse(lines);
        let n = items.len();
        if let Some(stack) = self.qf_stack_ensure(which) {
            stack.apply(items, action, title);
        }
        self.qf_refresh_window(which);
        Ok(n)
    }

    #[cfg(not(feature = "vim-regex"))]
    pub fn qf_set_from_lines(
        &mut self,
        _which: QfWhich,
        _lines: &[String],
        _efm: &str,
        _action: QfAction,
        _title: Option<String>,
    ) -> Result<usize, String> {
        Err("E: 'errorformat' parsing requires the vim-regex engine (not built)".to_string())
    }

    /// Populate the list from buffer `bufnr`'s lines parsed against the editor's
    /// `'errorformat'` (`:cbuffer`/`:lbuffer` and friends).
    pub fn qf_from_buffer(&mut self, which: QfWhich, bufnr: BufferId, action: QfAction) {
        let Some(ob) = self.buffers.map.get(&bufnr) else {
            self.echo(format!("E92: Buffer {} not found", bufnr.0));
            return;
        };
        let lines = ob.buffer.lines();
        let efm = self.options.errorformat.clone();
        let title = format!(":cbuffer {}", bufnr.0);
        match self.qf_set_from_lines(which, &lines, &efm, action, Some(title)) {
            Ok(n) => self.echo(format!("({}) {n} entries", which.label())),
            Err(e) => self.echo(e),
        }
    }

    /// `:cbuffer`/`:cgetbuffer`/`:caddbuffer [bufnr]` (and the `:l*` twins) —
    /// populate `which` from a buffer (current if no argument).
    pub(crate) fn ex_cbuffer(&mut self, which: QfWhich, args: &str, action: QfAction) {
        let bufnr = if args.trim().is_empty() {
            self.current_buffer_id()
        } else {
            match self.resolve_buffer(args) {
                Some(id) => id,
                None => return, // resolve_buffer already echoed the error
            }
        };
        self.qf_from_buffer(which, bufnr, action);
    }

    /// `:cfile`/`:cgetfile`/`:caddfile {file}` (and the `:l*` twins) — read `file`
    /// off the host fs (or a loaded buffer's live contents), parse it against the
    /// editor's `'errorformat'`, and populate `which`. `open`/`jump` mirror the
    /// `:make` post-populate behavior: `:cfile` opens + jumps, `:cgetfile` only
    /// fills, `:caddfile` appends without jumping.
    pub(crate) fn ex_cfile(
        &mut self,
        which: QfWhich,
        args: &str,
        action: QfAction,
        open: bool,
        jump: bool,
    ) {
        let file = args.trim();
        if file.is_empty() {
            self.echo("E471: Argument required".to_string());
            return;
        }
        // Expand `%`/`#` (and `:h`/`:t`/… mods) so `:cfile %` reads the current
        // file; a bad token fails loud.
        let Some(file) = self.expand_file_arg_or_echo(file) else {
            return;
        };
        let Some(lines) = self.vimgrep_file_lines(Path::new(&file)) else {
            return; // vimgrep_file_lines already echoed E484
        };
        let efm = self.options.errorformat.clone();
        let title = format!(":cfile {file}");
        match self.qf_set_from_lines(which, &lines, &efm, action, Some(title)) {
            Ok(n) => {
                self.echo(format!("({}) {n} entries", which.label()));
                if open || jump {
                    self.qf_post_populate(which, open, jump);
                }
            }
            Err(e) => self.echo(e),
        }
    }

    // ---- the display window + navigation ----

    /// True when the focused buffer is a quickfix **or** location-list display
    /// buffer — both are read-only (the `modifiable()` chokepoint consults this,
    /// and `input()` routes a special `<CR>`).
    pub(crate) fn is_quickfix_buffer(&self) -> bool {
        self.qf_context_of_buffer(self.current_buffer_id())
            .is_some()
    }

    /// Apply a named `qf` action, dispatched by a `FileType qf` buffer-local keymap
    /// (the default `<CR>` map in `prelude/keymap.lua`, or a user override) while a
    /// quickfix / location-list display buffer is focused. `jump` jumps to the entry
    /// on the cursor's line — vim's buffer-local quickfix `<CR>` mapping, now an
    /// ordinary (overridable) buffer-local map rather than a hard-coded `input()`
    /// branch. An unknown name fails loud per the no-silent-stub rule. The display
    /// buffer is otherwise an ordinary `nomodifiable` window (motions / search / `:`
    /// flow through, edits refused at the `modifiable()` chokepoints).
    pub fn apply_qf_action(&mut self, action: &str) -> Result<(), String> {
        match action {
            "jump" => {
                if let Some(which) = self.qf_context_of_buffer(self.current_buffer_id()) {
                    self.qf_jump_to_index(which, self.cursor.line);
                }
                Ok(())
            }
            other => Err(format!("unknown quickfix action {other:?}")),
        }
    }

    /// Buffer `buf`'s **buftype** — vim's buffer-kind noun, as the string the
    /// `buftype` option reports. nxvim models the kinds it actually distinguishes:
    /// `"quickfix"` for a quickfix **or** location-list display buffer (both report
    /// `quickfix` in vim), `"terminal"` for a terminal buffer, and `""` for an ordinary
    /// file / scratch buffer. This is what `nx.decor`'s `bufs.buftype` filter keys off
    /// (so a provider can target — or avoid — the quickfix window). Other vim buftypes
    /// (`help`, `nofile`, `prompt`, …) aren't modelled yet, so they read as `""`.
    pub fn buffer_buftype(&self, buf: BufferId) -> &'static str {
        if self.qf_context_of_buffer(buf).is_some() {
            "quickfix"
        } else if self.buffer_of(buf).is_some_and(|b| b.is_terminal()) {
            "terminal"
        } else {
            ""
        }
    }

    /// Which list a display buffer projects: the quickfix list if it is
    /// [`Editor::qf_bufnr`], else the location list of the window that owns it
    /// (the unique window whose `loclist_bufnr` is `buf`). `None` for an ordinary
    /// buffer.
    pub(crate) fn qf_context_of_buffer(&self, buf: BufferId) -> Option<QfWhich> {
        if self.qf_bufnr == Some(buf) {
            return Some(QfWhich::Quickfix);
        }
        self.window_ids()
            .into_iter()
            .find(|&w| {
                self.window(w)
                    .is_some_and(|win| win.loclist_bufnr == Some(buf))
            })
            .map(QfWhich::Location)
    }

    /// The display buffer backing `which`'s window, if one has been created.
    fn qf_display_bufnr(&self, which: QfWhich) -> Option<BufferId> {
        match which {
            QfWhich::Quickfix => self.qf_bufnr,
            QfWhich::Location(w) => self.window(w).and_then(|win| win.loclist_bufnr),
        }
    }

    /// Record `buf` as `which`'s display buffer (or clear it with `None`).
    fn qf_set_display_bufnr(&mut self, which: QfWhich, buf: Option<BufferId>) {
        match which {
            QfWhich::Quickfix => self.qf_bufnr = buf,
            QfWhich::Location(w) => {
                if let Some(win) = self.window_mut(w) {
                    win.loclist_bufnr = buf;
                }
            }
        }
    }

    /// The window currently showing `which`'s display buffer, if any.
    fn qf_window_id(&self, which: QfWhich) -> Option<WindowId> {
        let disp = self.qf_display_bufnr(which)?;
        self.window_ids()
            .into_iter()
            .find(|&w| self.window(w).is_some_and(|win| win.buffer == disp))
    }

    /// (Re)render `which`'s display buffer from its current list. No-op until the
    /// buffer exists (the window has been opened at least once).
    pub(crate) fn qf_refresh_window(&mut self, which: QfWhich) {
        let Some(buf) = self.qf_display_bufnr(which) else {
            return;
        };
        if !self.buffers.map.contains_key(&buf) {
            self.qf_set_display_bufnr(which, None);
            return;
        }
        let text = self.qf_render_text(which);
        let name = match which {
            QfWhich::Quickfix => "[Quickfix List]",
            QfWhich::Location(_) => "[Location List]",
        };
        self.load_str_into(buf, Some(name.to_string()), &text);
    }

    /// `which`'s display text: one `file|lnum col N| message` line per entry.
    fn qf_render_text(&self, which: QfWhich) -> String {
        self.qf_cur(which)
            .items
            .iter()
            .map(qf_render_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `:copen`/`:lopen [height]` — open (or focus) `which`'s window: a full-width
    /// split at the bottom (vim's `botright`), `height` rows (`10` by default). The
    /// display buffer is created on first use. For a location list the owner window
    /// is `which`'s window, so `<CR>`/`:ll` jump back into it.
    pub(crate) fn ex_qf_open(&mut self, which: QfWhich, args: &str) {
        let needs_buf = match self.qf_display_bufnr(which) {
            Some(id) => !self.buffers.map.contains_key(&id),
            None => true,
        };
        if needs_buf {
            let id = self.add_buffer(Buffer::empty());
            // vim's quickfix/loclist display buffer is `filetype=qf`. Setting it
            // makes its `FileType qf` autocmd fire (installing the buffer-local
            // `<CR>` jump map — the unified special-buffer model) and `:set ft?`
            // report `qf`, exactly as in vim.
            self.set_filetype(id, "qf");
            self.qf_set_display_bufnr(which, Some(id));
        }
        self.qf_refresh_window(which);
        if let Some(w) = self.qf_window_id(which) {
            self.set_current_window(w);
            return;
        }
        let height = args
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&h| h > 0)
            .unwrap_or(10);
        self.qf_prev_win = Some(self.windows.current);
        let disp = self
            .qf_display_bufnr(which)
            .expect("display buffer created above");
        if self.options.qfdock {
            self.qf_place_in_dock(disp, height);
        } else {
            self.open_bottom_window(disp, height);
        }
    }

    /// Host `disp` (a `filetype=qf` display buffer) as a tab in the **bottom dock** —
    /// the nxvim way (`'qfdock'`). The first such list opens the dock; subsequent
    /// lists add a tab beside it, so several searches sit side by side. `qf_prev_win`
    /// is already set to the invoking (main-layer) window, so `<CR>` jumps back into
    /// the main layer (see [`Editor::qf_focus_target_window`]).
    fn qf_place_in_dock(&mut self, disp: BufferId, height: usize) {
        if self.dock_is_open(DockSide::Bottom) {
            self.focus_dock(DockSide::Bottom);
            self.new_tab(disp, WindowOptions::default());
        } else {
            self.open_dock(DockSide::Bottom, height, Some(disp));
        }
    }

    /// Send `items` to a **new** location list shown as its own tab in the bottom
    /// dock — the nxvim "save this search" surface. The new tab's window both *owns*
    /// and *displays* the list, so every call adds an independent list beside the
    /// existing ones (unlike the single global quickfix list). `<CR>` on an entry
    /// jumps into the main editing layer: the owner is the dock window, which is the
    /// display window too, so it is excluded as the jump target and the
    /// [`Editor::qf_focus_target_window`] fallback lands in the main layer (which
    /// `open_layers` always lists first). Returns the owning/display window.
    pub fn loclist_to_dock(&mut self, items: Vec<QfEntry>, title: String) -> WindowId {
        let disp = self.add_buffer(Buffer::empty());
        // `filetype=qf` installs the buffer-local `<CR>` jump map and the qf render.
        self.set_filetype(disp, "qf");
        self.qf_place_in_dock(disp, DockSide::Bottom.default_size());
        let w = self.windows.current;
        self.qf_set_display_bufnr(QfWhich::Location(w), Some(disp));
        self.qf_set_items(QfWhich::Location(w), items, QfAction::New, Some(title));
        w
    }

    /// Send `items` to a quickfix or location list and show it — the engine behind
    /// `nx.qf.{send,add}_to_{loc,qf}list` (the picker's quickfix-style sinks, the
    /// nxvim port of telescope's send/add-to-list actions).
    ///
    /// - `to_qf`: the single global **quickfix** list (one window/tab, reused) vs a
    ///   **location** list.
    /// - `action`: [`QfAction::New`] for a *send* (a fresh list), [`QfAction::Add`]
    ///   for an *add* (append).
    ///
    /// Honors `'qfdock'`. With it off, both kinds open the classic way — a bottom
    /// split of the current window. With it on (the nxvim default): the quickfix list
    /// shows as its (single) bottom-dock tab; a location-list *send* opens a **new**
    /// dock tab beside the others ([`Editor::loclist_to_dock`]), and a location-list
    /// *add* appends to the focused dock loclist tab when one is focused (else falls
    /// back to a new tab — there is nothing to append to).
    pub fn list_send(&mut self, items: Vec<QfEntry>, title: String, action: QfAction, to_qf: bool) {
        if to_qf {
            // One global quickfix list: replace/append, then show (dock tab or split).
            self.qf_set_items(QfWhich::Quickfix, items, action, Some(title));
            self.ex_qf_open(QfWhich::Quickfix, "");
            return;
        }
        if !self.options.qfdock {
            // Classic vim/telescope: the current window's location list + a split.
            let which = QfWhich::Location(self.windows.current);
            self.qf_set_items(which, items, action, Some(title));
            self.ex_qf_open(which, "");
            return;
        }
        // Dock mode. An `add` appends to the focused dock loclist tab if we are on
        // one; every `send` (and an `add` with no list focused) opens a fresh tab.
        if matches!(action, QfAction::Add) {
            if let Some(which @ QfWhich::Location(_)) =
                self.qf_context_of_buffer(self.current_buffer_id())
            {
                self.qf_set_items(which, items, QfAction::Add, Some(title));
                return;
            }
        }
        self.loclist_to_dock(items, title);
    }

    /// `:cclose`/`:lclose` — close `which`'s window if open (leaving focus on a
    /// code window).
    pub(crate) fn ex_qf_close(&mut self, which: QfWhich) {
        let Some(w) = self.qf_window_id(which) else {
            return;
        };
        let prev = self.windows.current;
        self.set_current_window(w);
        // A dock-hosted display (the `'qfdock'` way) closes as a *tab* — which tears
        // the dock down on its last tab — not a window: the dock's last-window guard
        // would otherwise refuse a `close_window`.
        if matches!(self.tree_of_window(w), Some((Layer::Dock(_), _))) {
            self.close_tab();
        } else {
            self.close_window();
        }
        if prev != w && self.window_ids().contains(&prev) {
            self.set_current_window(prev);
        }
    }

    /// `:cwindow`/`:lwindow` — open `which`'s window iff its list is non-empty,
    /// else close it.
    pub(crate) fn ex_qf_window(&mut self, which: QfWhich, args: &str) {
        if self.qf_cur(which).items.is_empty() {
            self.ex_qf_close(which);
        } else {
            self.ex_qf_open(which, args);
        }
    }

    /// `:cc`/`:ll [nr]` — jump to entry `nr` (1-based; current when omitted).
    pub(crate) fn ex_qf_cc(&mut self, which: QfWhich, nr: Option<usize>) {
        let list = self.qf_cur(which);
        if list.items.is_empty() {
            self.echo("E42: No Errors".to_string());
            return;
        }
        let last = list.items.len() - 1;
        let idx = match nr {
            Some(n) => n.saturating_sub(1),
            None => list.idx.saturating_sub(1),
        };
        self.qf_jump_to_index(which, idx.min(last));
    }

    /// `:cnext`/`:cprev` (and `:l*` twins) — step `count` *valid* entries forward /
    /// backward and jump there. `E553` past either end.
    pub(crate) fn ex_qf_step(&mut self, which: QfWhich, forward: bool, count: usize) {
        let list = self.qf_cur(which);
        if !list.items.iter().any(|e| e.valid) {
            self.echo("E42: No Errors".to_string());
            return;
        }
        let len = list.items.len() as isize;
        let step: isize = if forward { 1 } else { -1 };
        let mut pos = list.idx as isize - 1; // 0-based current (-1 if unset)
        let mut remaining = count.max(1);
        let valid: Vec<bool> = list.items.iter().map(|e| e.valid).collect();
        while remaining > 0 {
            pos += step;
            if pos < 0 || pos >= len {
                self.echo("E553: No more items".to_string());
                return;
            }
            if valid[pos as usize] {
                remaining -= 1;
            }
        }
        self.qf_jump_to_index(which, pos as usize);
    }

    /// `:cnfile`/`:cpfile` (and `:l*` twins) — jump to the first error in the next
    /// file forward / the last error in the previous file backward, stepping
    /// `count` files. Files are identified by an entry's resolved target (its
    /// filename, or its buffer number for a bufnr-only entry); entries with neither
    /// (plain output lines) are skipped. `E553` past either end.
    pub(crate) fn ex_qf_step_file(&mut self, which: QfWhich, forward: bool, count: usize) {
        // `keys` is owned, so no borrow of `self` is held across the jump below.
        let keys: Vec<Option<String>> = self.qf_cur(which).items.iter().map(qf_file_key).collect();
        if !keys.iter().any(Option::is_some) {
            self.echo("E42: No Errors".to_string());
            return;
        }
        let len = keys.len() as isize;
        let step: isize = if forward { 1 } else { -1 };
        let mut pos = self.qf_cur(which).idx as isize - 1; // 0-based current (-1 if unset)
        let mut prev_key = (pos >= 0).then(|| keys[pos as usize].clone()).flatten();
        let mut remaining = count.max(1);
        loop {
            pos += step;
            if pos < 0 || pos >= len {
                self.echo("E553: No more items".to_string());
                return;
            }
            let Some(key) = &keys[pos as usize] else {
                continue; // a fileless (invalid) line — not part of any file group
            };
            if Some(key) != prev_key.as_ref() {
                // Crossed into a new file. Going forward `pos` is its first entry;
                // going backward it is the new file's *last* entry — exactly vim's
                // "last error in the previous file".
                remaining -= 1;
                prev_key = Some(key.clone());
                if remaining == 0 {
                    self.qf_jump_to_index(which, pos as usize);
                    return;
                }
            }
        }
    }

    /// `:cfirst`/`:clast` (and `:l*` twins) — jump to the first / last valid entry.
    pub(crate) fn ex_qf_first(&mut self, which: QfWhich) {
        match self.qf_cur(which).items.iter().position(|e| e.valid) {
            Some(i) => self.qf_jump_to_index(which, i),
            None => self.echo("E42: No Errors".to_string()),
        }
    }

    pub(crate) fn ex_qf_last(&mut self, which: QfWhich) {
        match self.qf_cur(which).items.iter().rposition(|e| e.valid) {
            Some(i) => self.qf_jump_to_index(which, i),
            None => self.echo("E42: No Errors".to_string()),
        }
    }

    /// `:colder`/`:cnewer` (and `:lolder`/`:lnewer`) — walk `which`'s list stack
    /// `count` steps toward older / newer, then re-render the window and echo the
    /// new position. `E380`/`E381` at the ends; `E380` too when the stack is empty.
    pub(crate) fn ex_qf_history(&mut self, which: QfWhich, newer: bool, count: usize) {
        let Some(stack) = self.qf_stack_ensure(which) else {
            self.echo("E776: No location list".to_string());
            return;
        };
        if stack.lists.is_empty() {
            self.echo("E380: At bottom of quickfix stack".to_string());
            return;
        }
        let moved = if newer {
            stack.newer(count)
        } else {
            stack.older(count)
        };
        if !moved {
            self.echo(if newer {
                "E381: At top of quickfix stack".to_string()
            } else {
                "E380: At bottom of quickfix stack".to_string()
            });
            return;
        }
        let (cur, len, n, title) = {
            let s = self.qf_stack(which).expect("stack present");
            let list = s.current().expect("non-empty");
            (
                s.cur + 1,
                s.lists.len(),
                list.items.len(),
                list.title.clone(),
            )
        };
        self.qf_refresh_window(which);
        let label = which.label();
        self.echo(format!("{title} ({label} list {cur} of {len}); {n} items"));
    }

    /// The post-populate step `:make`/`:grep`/`:cfile` add after filling a list:
    /// open the window iff there are entries (`open`, vim's `:cwindow`), then jump
    /// to the first valid entry (`jump`, suppressed by a `!`). A clean run opens
    /// nothing and jumps nowhere.
    pub fn qf_post_populate(&mut self, which: QfWhich, open: bool, jump: bool) {
        if open {
            self.ex_qf_window(which, "");
        }
        if jump && self.qf_cur(which).items.iter().any(|e| e.valid) {
            self.ex_qf_first(which);
        }
    }

    /// Jump to entry `idx` (0-based) of `which`: mark it current, focus a code
    /// window per `'switchbuf'`, and land the cursor at the entry's
    /// `file:line:col`.
    pub(crate) fn qf_jump_to_index(&mut self, which: QfWhich, idx: usize) {
        let Some(entry) = self.qf_cur(which).items.get(idx).cloned() else {
            self.echo("E42: No Errors".to_string());
            return;
        };
        if let Some(list) = self.qf_cur_mut(which) {
            list.idx = idx + 1;
        }
        // Resolve the target file: the entry's `filename`, or — for a buffer-number
        // -addressed entry (e.g. `setqflist`/diagnostics that carry only `bufnr`) —
        // that buffer's path. vim jumps to either.
        let target = entry.filename.clone().or_else(|| {
            (entry.bufnr > 0)
                .then(|| self.buffers.map.get(&BufferId(entry.bufnr as u64)))
                .flatten()
                .and_then(|ob| ob.buffer.path.as_ref())
                .map(|p| p.to_string_lossy().into_owned())
        });
        let Some(filename) = target else {
            // A non-error line (no file/buffer): echo its text, like vim's E42-free
            // no-op.
            self.echo(entry.text.clone());
            return;
        };
        self.qf_focus_target_window(which);
        let line0 = entry.lnum.saturating_sub(1);
        let col0 = entry.col.saturating_sub(1);
        self.jump_to(Path::new(&filename), line0, col0);
    }

    /// Move focus to the window a jump should land in, honoring `'switchbuf'`. From
    /// the display window, step to the code window: for the quickfix list the one
    /// `:copen` was invoked from (else any non-display window), for a location list
    /// its **owner** window (the loclist belongs to it). Then a `split`/`vsplit`
    /// `'switchbuf'` value opens a new window for the jump. (`useopen`/`usetab` are
    /// acted on downstream by [`Editor::jump_to`], which this jump funnels through —
    /// so a quickfix jump to a buffer already shown elsewhere reuses that window.)
    fn qf_focus_target_window(&mut self, which: QfWhich) {
        if self.is_quickfix_buffer() {
            let disp_win = self.qf_window_id(which);
            let live = self.window_ids();
            let preferred = match which {
                QfWhich::Quickfix => self.qf_prev_win,
                QfWhich::Location(owner) => Some(owner),
            };
            let target = preferred
                .filter(|w| live.contains(w) && Some(*w) != disp_win)
                .or_else(|| live.into_iter().find(|w| Some(*w) != disp_win));
            match target {
                Some(w) => self.set_current_window(w),
                None => self.split(SplitDir::Horizontal),
            }
        }
        let swb = self.options.switchbuf.clone();
        if swb.split(',').any(|s| s == "vsplit") {
            self.split(SplitDir::Vertical);
        } else if swb.split(',').any(|s| s == "split") {
            self.split(SplitDir::Horizontal);
        }
    }
}

// ---------------------------------------------------------------------------
// `:vimgrep` — the in-process search producer (Phase 3).

impl Editor {
    /// `:vimgrep[!] /{pattern}/[g][j] {file} …` (and `:vimgrepadd`, which appends).
    /// Search each named file for `{pattern}` using the active `'regexsyntax'`
    /// engine — honoring `'ignorecase'`/`'smartcase'` exactly like `/` — and add a
    /// quickfix entry for the first match on each line, or for *every* match with
    /// the `g` flag. Unless the `j` flag is given, jump to the first match. No
    /// external process is involved, so this works on every build (including the
    /// web edit-host). File globbing is not yet supported: a path with a glob
    /// metacharacter fails loud rather than silently matching nothing.
    pub(crate) fn ex_vimgrep(&mut self, which: QfWhich, args: &str, action: QfAction) {
        let Some((pattern, every, jump, files)) = self.parse_vimgrep_args(args) else {
            return;
        };
        // Globbing is deferred (Phase 4); a glob argument fails loud and aborts the
        // whole command rather than searching the rest and reporting a misleading
        // "no match".
        if let Some(g) = files.iter().find(|f| is_glob(f)) {
            self.echo(format!(
                "E: :vimgrep file globbing is not yet supported: {g}"
            ));
            return;
        }
        let ic = self.search_ignorecase(&pattern);
        let re = match crate::search::SearchRegex::compile(&pattern, ic, self.search_engine()) {
            Ok(re) => re,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        let mut entries = Vec::new();
        for file in &files {
            let path = Path::new(file);
            let Some(lines) = self.vimgrep_file_lines(path) else {
                continue;
            };
            for (i, line) in lines.iter().enumerate() {
                let spans = if every {
                    re.find_all(line)
                } else {
                    re.find_from(line, 0).into_iter().collect()
                };
                for (start, _end) in spans {
                    entries.push(QfEntry {
                        filename: Some(file.clone()),
                        lnum: i + 1,
                        col: start + 1,
                        text: line.clone(),
                        nr: -1,
                        valid: true,
                        ..QfEntry::default()
                    });
                }
            }
        }
        let n = entries.len();
        let title = format!(":vimgrep {}", args.trim());
        self.qf_set_items(which, entries, action, Some(title));
        if n == 0 {
            self.echo(format!("E480: No match: {pattern}"));
            return;
        }
        self.echo(format!("({}) {n} matches", which.label()));
        if jump {
            let idx = self.qf_cur(which).idx.saturating_sub(1);
            self.qf_jump_to_index(which, idx);
        }
    }

    /// Parse a `:vimgrep` argument into `(pattern, every, jump, files)`. The pattern
    /// is delimited by its first non-keyword character (`/.../`, `#...#`, …); a
    /// bare leading word is taken as the pattern up to the first blank (vim's
    /// separator-less form). Flags `g` (every match per line) and `j` (don't jump)
    /// follow the closing delimiter. Returns `None` (after echoing) on a malformed
    /// or empty argument.
    fn parse_vimgrep_args(&mut self, args: &str) -> Option<(String, bool, bool, Vec<String>)> {
        let s = args.trim();
        if s.is_empty() {
            self.echo("E471: Argument required".to_string());
            return None;
        }
        let first = s.chars().next().unwrap();
        let (pattern, mut every, mut jump, rest);
        if first.is_alphanumeric() || first == '_' || first == '\\' {
            // Separator-less form: the pattern is the first blank-delimited word.
            let end = s.find(char::is_whitespace).unwrap_or(s.len());
            pattern = s[..end].to_string();
            every = false;
            jump = true;
            rest = s[end..].trim_start().to_string();
        } else {
            // Delimited form: find the matching unescaped closing delimiter.
            let delim = first;
            let bytes = s.as_bytes();
            let mut i = 1;
            let mut close = None;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] as char == delim {
                    close = Some(i);
                    break;
                }
                i += 1;
            }
            let Some(close) = close else {
                self.echo(format!("E682: Invalid search pattern or delimiter: {s}"));
                return None;
            };
            pattern = s[1..close].to_string();
            // Flags run from just after the delimiter up to the first blank.
            let after = &s[close + 1..];
            let fend = after.find(char::is_whitespace).unwrap_or(after.len());
            every = false;
            jump = true;
            for c in after[..fend].chars() {
                match c {
                    'g' => every = true,
                    'j' => jump = false,
                    _ => {}
                }
            }
            rest = after[fend..].trim_start().to_string();
        }
        if pattern.is_empty() {
            self.echo("E35: No previous regular expression".to_string());
            return None;
        }
        // Expand `%` (current file) / `#` (alternate) plus their `:h`/`:t`/… mods in
        // each file argument, exactly as vim does for `:vimgrep …  %`. A bad token
        // (no name to substitute) fails loud and aborts the command.
        let mut files = Vec::new();
        for tok in rest.split_whitespace() {
            match self.expand_file_arg(tok) {
                Ok(f) => files.push(f),
                Err(e) => {
                    self.echo(e);
                    return None;
                }
            }
        }
        if files.is_empty() {
            self.echo("E471: Argument required: file name".to_string());
            return None;
        }
        Some((pattern, every, jump, files))
    }

    /// The lines of `path` for `:vimgrep`: the live contents if a buffer is already
    /// loaded on it (so unsaved edits are searched, and it works without disk I/O),
    /// else read fresh from disk through the host fs. Echoes and returns `None` on a
    /// read failure.
    fn vimgrep_file_lines(&mut self, path: &Path) -> Option<Vec<String>> {
        if let Some(id) = self.find_buffer_by_path(path) {
            return Some(self.buffers.get(id).buffer.lines());
        }
        let fs = self.host_fs();
        match Buffer::from_file(path, &*fs, &self.options.fileencodings) {
            Ok(b) => Some(b.lines()),
            Err(e) => {
                self.echo(format!("E484: Can't open file {}: {e}", path.display()));
                None
            }
        }
    }
}

/// Whether `arg` carries a shell/file glob metacharacter (`*`, `?`, `[`). Globbing
/// in `:vimgrep` file arguments is deferred (Phase 4); such an argument fails loud.
fn is_glob(arg: &str) -> bool {
    arg.contains('*') || arg.contains('?') || arg.contains('[')
}

/// A stable per-file identity for an entry, used by `:cnfile`/`:cpfile` to group
/// entries by file: its `filename`, or `\0buf<n>` for a buffer-number-only entry.
/// `None` for a fileless (plain-output / invalid) line, which belongs to no file.
fn qf_file_key(e: &QfEntry) -> Option<String> {
    e.filename
        .clone()
        .or_else(|| (e.bufnr > 0).then(|| format!("\0buf{}", e.bufnr)))
}

/// Render one quickfix entry as a `:copen` line: `file|lnum col N| message`
/// (vim's default format). A non-error line drops the empty location: `|| text`.
fn qf_render_line(e: &QfEntry) -> String {
    let fname = e.filename.as_deref().unwrap_or("");
    let mut loc = String::new();
    if e.lnum > 0 {
        loc.push_str(&e.lnum.to_string());
        if e.col > 0 {
            loc.push_str(" col ");
            loc.push_str(&e.col.to_string());
        }
    }
    let text = e.text.replace('\n', " ");
    format!("{fname}|{loc}| {text}")
}

// ---------------------------------------------------------------------------
// The errorformat engine (vim regexp engine required).

#[cfg(feature = "vim-regex")]
pub(crate) use engine::Errorformat;

#[cfg(feature = "vim-regex")]
mod engine {
    use super::QfEntry;
    use nxvim_regex::{Engine, PatternKind, VimRegex};
    use std::path::Path;

    // The 'errorformat' conversion characters, in vim's `fmt_pat[]` order. The
    // index of each is its capture-group ordinal source and its `qf_parse_fmt`
    // slot. Keep in sync with `PATTERNS` below.
    const CONV: [u8; 14] = [
        b'f', b'b', b'n', b'l', b'e', b'c', b'k', b't', b'm', b'r', b'p', b'v', b's', b'o',
    ];
    // The regex fragment each conversion expands to (vim's `fmt_pat[].pattern`).
    // `%f` is special-cased in `efm_part_to_regpat` and never reads its slot here.
    const PATTERNS: [&str; 14] = [
        ".\\+",     // f (only used when %f is at the end)
        "\\d\\+",   // b
        "\\d\\+",   // n
        "\\d\\+",   // l
        "\\d\\+",   // e
        "\\d\\+",   // c
        "\\d\\+",   // k
        ".",        // t
        ".\\+",     // m
        ".*",       // r
        "[-\t .]*", // p
        "\\d\\+",   // v
        ".\\+",     // s
        ".\\+",     // o
    ];
    // Named field indices into `CONV`/`PATTERNS`/`EfmPattern::addr`.
    const I_F: usize = 0;
    const I_M: usize = 8; // FMT_PATTERN_M
    const I_R: usize = 9; // FMT_PATTERN_R

    fn fmt_index(c: u8) -> Option<usize> {
        CONV.iter().position(|&p| p == c)
    }

    /// One compiled `'errorformat'` part.
    struct EfmPattern {
        prog: VimRegex,
        /// The leading prefix char (`A E W I N C Z G O P Q D X`), `0` if none.
        prefix: u8,
        /// The `%+` / `%-` flag, `0` if none.
        flags: u8,
        /// `%>`: continue matching the *next* line from this pattern.
        conthere: bool,
        /// Field index → 1-based capture-group number (`0` = field absent), in the
        /// `CONV` order. Mirrors vim's `efm_T.addr`.
        addr: [usize; 14],
    }

    /// The full compiled `'errorformat'`: an ordered list of parts.
    pub(crate) struct Errorformat {
        parts: Vec<EfmPattern>,
    }

    impl Errorformat {
        /// Compile an `'errorformat'` string (comma-separated parts). Returns the
        /// `E37x` message vim would emit for a malformed part.
        pub(crate) fn compile(efm: &str) -> Result<Self, String> {
            let bytes = efm.as_bytes();
            let mut parts = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                let len = part_len(&bytes[i..]);
                if len > 0 {
                    parts.push(EfmPattern::compile(&bytes[i..i + len])?);
                }
                // Skip the comma and any following blanks (vim's
                // `skip_to_option_part`).
                i += len;
                while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ') {
                    i += 1;
                }
            }
            if parts.is_empty() {
                return Err("E378: 'errorformat' contains no pattern".to_string());
            }
            Ok(Errorformat { parts })
        }

        /// Parse `lines` into quickfix entries.
        pub(crate) fn parse(&self, lines: &[String]) -> Vec<QfEntry> {
            let mut p = Parser::new(&self.parts);
            for line in lines {
                p.parse_line(line);
            }
            p.entries
        }
    }

    /// Length of one `'errorformat'` part — up to the next unescaped comma (vim's
    /// `efm_option_part_len`).
    fn part_len(efm: &[u8]) -> usize {
        let mut len = 0;
        while len < efm.len() && efm[len] != b',' {
            if efm[len] == b'\\' && len + 1 < efm.len() {
                len += 1;
            }
            len += 1;
        }
        len
    }

    impl EfmPattern {
        fn compile(part: &[u8]) -> Result<Self, String> {
            let (regpat, addr, prefix, flags, conthere) = efm_part_to_regpat(part)?;
            let prog = VimRegex::compile_with(&regpat, PatternKind::String, Engine::Auto)
                .map_err(|e| format!("E383: errorformat regex compile failed: {e}"))?;
            Ok(EfmPattern {
                prog,
                prefix,
                flags,
                conthere,
                addr,
            })
        }
    }

    /// Port of vim's `efm_to_regpat`: convert one `'errorformat'` part to a vim
    /// regex pattern, returning the pattern plus the parsed prefix/flags/addr.
    fn efm_part_to_regpat(part: &[u8]) -> Result<(String, [usize; 14], u8, u8, bool), String> {
        let n = part.len();
        let mut out: Vec<u8> = Vec::with_capacity(n * 4 + 16);
        out.push(b'^');
        let mut addr = [0usize; 14];
        let mut prefix = 0u8;
        let mut flags = 0u8;
        let mut conthere = false;
        let mut round = 0usize;

        let mut i = 0;
        while i < n {
            let c = part[i];
            if c != b'%' {
                // Copy a normal character, escaping regex atoms — and treating a
                // backslash as "take the next char literally" (vim's behavior).
                if c == b'\\' && i + 1 < n {
                    i += 1;
                    out.push(part[i]);
                } else {
                    if matches!(c, b'.' | b'*' | b'^' | b'$' | b'~' | b'[') {
                        out.push(b'\\');
                    }
                    out.push(c);
                }
                i += 1;
                continue;
            }

            // A '%' item.
            i += 1;
            if i >= n {
                return Err("E377: Invalid % in format string".to_string());
            }
            let cv = part[i];
            if let Some(idx) = fmt_index(cv) {
                efmpat_to_regpat(part, i, idx, prefix, &mut addr, &mut round, &mut out)?;
            } else if cv == b'*' {
                i += 1;
                if i >= n {
                    return Err("E375: Unsupported % in format string".to_string());
                }
                i = scanf_fmt_to_regpat(part, i, &mut out)?;
            } else if matches!(cv, b'%' | b'\\' | b'.' | b'^' | b'$' | b'~' | b'[') {
                out.push(cv); // regex magic characters, passed through
            } else if cv == b'#' {
                out.push(b'*');
            } else if cv == b'>' {
                conthere = true;
            } else if i == 1 {
                // A prefix — only valid at the very start of the part.
                i = efm_analyze_prefix(part, i, &mut prefix, &mut flags)?;
            } else {
                return Err(format!("E377: Invalid %%{} in format string", cv as char));
            }
            i += 1;
        }
        out.push(b'$');
        let pat = String::from_utf8(out)
            .map_err(|_| "E383: errorformat produced non-UTF-8 pattern".to_string())?;
        Ok((pat, addr, prefix, flags, conthere))
    }

    /// Port of `efmpat_to_regpat`: expand one field conversion (`part[at]` is the
    /// conversion char, `idx` its `CONV` index) into a `\(…\)` capture group.
    fn efmpat_to_regpat(
        part: &[u8],
        at: usize,
        idx: usize,
        prefix: u8,
        addr: &mut [usize; 14],
        round: &mut usize,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let cv = part[at];
        if addr[idx] != 0 {
            return Err(format!("E372: Too many %%{} in format string", cv as char));
        }
        let dxopq = matches!(prefix, b'D' | b'X' | b'O' | b'P' | b'Q');
        let opq = matches!(prefix, b'O' | b'P' | b'Q');
        if (idx != 0 && idx < I_R && dxopq) || (idx == I_R && !opq) {
            return Err(format!(
                "E373: Unexpected %%{} in format string",
                cv as char
            ));
        }
        *round += 1;
        addr[idx] = *round;
        out.push(b'\\');
        out.push(b'(');
        if cv == b'f' && at + 1 < part.len() {
            // A filename followed by more pattern: greedily-minimal up to the next
            // literal (`.\{-1,}`), or `\f\+` when the next item is `\`/`%`.
            let nxt = part[at + 1];
            if nxt != b'\\' && nxt != b'%' {
                out.extend_from_slice(b".\\{-1,}");
            } else {
                out.extend_from_slice(b"\\f\\+");
            }
        } else {
            out.extend_from_slice(PATTERNS[idx].as_bytes());
        }
        out.push(b'\\');
        out.push(b')');
        Ok(())
    }

    /// Port of `scanf_fmt_to_regpat` for `%*…`: `part[at]` is the char after `*`.
    /// Returns the index of the last consumed byte.
    fn scanf_fmt_to_regpat(part: &[u8], at: usize, out: &mut Vec<u8>) -> Result<usize, String> {
        let n = part.len();
        let mut i = at;
        let c = part[i];
        if c == b'[' {
            out.push(b'['); // %*[^a-z0-9] etc.
            if i + 1 < n && part[i + 1] == b'^' {
                i += 1;
                out.push(part[i]); // '^'
            }
            if i + 1 < n {
                i += 1;
                out.push(part[i]); // could be ']'
                loop {
                    if i + 1 >= n {
                        return Err("E374: Missing ] in format string".to_string());
                    }
                    i += 1;
                    let ch = part[i];
                    out.push(ch);
                    if ch == b']' {
                        break;
                    }
                }
            }
            out.extend_from_slice(b"\\+");
        } else if c == b'\\' {
            out.push(b'\\'); // %*\D, %*\s etc.
            if i + 1 < n {
                i += 1;
                out.push(part[i]);
            }
            out.extend_from_slice(b"\\+");
        } else {
            return Err(format!(
                "E375: Unsupported %%*{} in format string",
                c as char
            ));
        }
        Ok(i)
    }

    /// Port of `efm_analyze_prefix`: read an optional `+`/`-` flag and the prefix
    /// letter starting at `part[at]`. Returns the index of the prefix letter.
    fn efm_analyze_prefix(
        part: &[u8],
        at: usize,
        prefix: &mut u8,
        flags: &mut u8,
    ) -> Result<usize, String> {
        let n = part.len();
        let mut i = at;
        if i < n && matches!(part[i], b'+' | b'-') {
            *flags = part[i];
            i += 1;
        }
        if i < n
            && matches!(
                part[i],
                b'D' | b'X'
                    | b'A'
                    | b'E'
                    | b'W'
                    | b'I'
                    | b'N'
                    | b'C'
                    | b'Z'
                    | b'G'
                    | b'O'
                    | b'P'
                    | b'Q'
            )
        {
            *prefix = part[i];
            Ok(i)
        } else {
            let bad = if i < n { part[i] as char } else { '?' };
            Err(format!("E376: Invalid %%{bad} in format string prefix"))
        }
    }

    // -----------------------------------------------------------------------
    // Line parsing (port of qf_parse_line / qf_parse_match / qf_parse_fmt_*).

    /// Scratch fields filled while parsing one line (vim's `qffields_T`).
    #[derive(Default)]
    struct Fields {
        namebuf: String,
        bnr: i32,
        module: String,
        errmsg: String,
        lnum: usize,
        end_lnum: usize,
        col: usize,
        end_col: usize,
        use_viscol: bool,
        pattern: String,
        enr: i32,
        typ: u8,
        valid: bool,
        /// Byte offset into the line where `%r` started (the "rest"), if matched.
        tail: Option<usize>,
    }

    impl Fields {
        fn reset(&mut self, keep_errmsg: bool) {
            self.namebuf.clear();
            self.bnr = 0;
            self.module.clear();
            self.pattern.clear();
            if !keep_errmsg {
                self.errmsg.clear();
            }
            self.lnum = 0;
            self.end_lnum = 0;
            self.col = 0;
            self.end_col = 0;
            self.use_viscol = false;
            self.enr = -1;
            self.typ = 0;
            self.tail = None;
        }
    }

    struct Parser<'a> {
        parts: &'a [EfmPattern],
        entries: Vec<QfEntry>,
        multiline: bool,
        multiignore: bool,
        multiscan: bool,
        directory: Option<String>,
        dir_stack: Vec<String>,
        currfile: Option<String>,
        file_stack: Vec<String>,
        /// Index of the `%>` pattern to resume from on the next line.
        fmt_start: Option<usize>,
    }

    impl<'a> Parser<'a> {
        fn new(parts: &'a [EfmPattern]) -> Self {
            Parser {
                parts,
                entries: Vec::new(),
                multiline: false,
                multiignore: false,
                multiscan: false,
                directory: None,
                dir_stack: Vec::new(),
                currfile: None,
                file_stack: Vec::new(),
                fmt_start: None,
            }
        }

        fn parse_line(&mut self, line: &str) {
            let mut fields = Fields::default();
            // A line may be re-scanned from a `%r`/`%O%P%Q` tail (vim's
            // `goto restofline`); bound the loop by the line shrinking each pass.
            let mut cur = line.to_string();
            loop {
                match self.parse_one(&cur, &mut fields) {
                    LineStatus::AddEntry => {
                        self.add_entry(&fields);
                        return;
                    }
                    LineStatus::Ignore => return,
                    LineStatus::Rescan(rest) => {
                        if rest.len() >= cur.len() {
                            return; // no progress — drop the line
                        }
                        cur = rest;
                    }
                }
            }
        }

        /// One pass of `qf_parse_line` over `line`.
        fn parse_one(&mut self, line: &str, fields: &mut Fields) -> LineStatus {
            // `%>` resume point, else the first pattern.
            let start = self.fmt_start.take().unwrap_or(0);
            fields.valid = true;

            let mut matched: Option<usize> = None;
            for fi in start..self.parts.len() {
                if self.parse_get_fields(line, &self.parts[fi], fields) {
                    matched = Some(fi);
                    break;
                }
            }
            self.multiscan = false;

            let Some(fi) = matched else {
                // No pattern matched: a plain output line. It still becomes an
                // (invalid) entry so `:copen` shows it.
                self.line_nomatch(line, fields);
                self.multiline = false;
                self.multiignore = false;
                return LineStatus::AddEntry;
            };

            let prefix = self.parts[fi].prefix;
            if prefix == b'D' || prefix == b'X' {
                if let Err(()) = self.parse_dir_pfx(prefix, fields) {
                    return LineStatus::Ignore;
                }
                self.line_nomatch(line, fields);
                return LineStatus::AddEntry;
            }

            if self.parts[fi].conthere {
                self.fmt_start = Some(fi);
            }

            if matches!(prefix, b'A' | b'E' | b'W' | b'I' | b'N') {
                self.multiline = true;
                self.multiignore = false;
            } else if matches!(prefix, b'C' | b'Z') {
                self.parse_multiline_pfx(prefix, fields);
                return LineStatus::Ignore;
            } else if matches!(prefix, b'O' | b'P' | b'Q') {
                if let Some(rest) = self.parse_file_pfx(prefix, fields, line) {
                    return LineStatus::Rescan(rest);
                }
            }

            if self.parts[fi].flags == b'-' {
                if self.multiline {
                    self.multiignore = true;
                }
                return LineStatus::Ignore;
            }
            LineStatus::AddEntry
        }

        /// Run one pattern against `line`, filling `fields` (vim's
        /// `qf_parse_get_fields` + `qf_parse_match`). Returns whether it matched.
        fn parse_get_fields(&self, line: &str, fmt: &EfmPattern, fields: &mut Fields) -> bool {
            if self.multiscan && !matches!(fmt.prefix, b'O' | b'P' | b'Q') {
                return false;
            }
            fields.reset(self.multiscan);

            let m = match fmt.prog.exec_line(line, 0, true) {
                Ok(Some(m)) => m,
                // A no-match or even an engine error means "this pattern doesn't
                // apply"; fall through to the next pattern (fail-soft per line).
                _ => return false,
            };

            // (C/Z) continuation only when already in a multi-line message.
            if matches!(fmt.prefix, b'C' | b'Z') && !self.multiline {
                return false;
            }
            fields.typ = if matches!(fmt.prefix, b'E' | b'W' | b'I' | b'N') {
                fmt.prefix
            } else {
                0
            };

            let sub = |g: usize| m.submatches.get(g).copied().flatten();
            for (i, &g) in fmt.addr.iter().enumerate() {
                if i == I_F {
                    if g > 0 {
                        let Some((s, e)) = sub(g) else { return false };
                        // Filename: literal slice (env expansion is deferred).
                        fields.namebuf = line[s..e].to_string();
                    }
                    continue;
                }
                if i == I_M {
                    if fmt.flags == b'+' && !self.multiscan {
                        fields.errmsg = line.to_string();
                    } else if g > 0 {
                        let Some((s, e)) = sub(g) else { return false };
                        fields.errmsg = line[s..e].to_string();
                    }
                    continue;
                }
                if i == I_R {
                    if g > 0 {
                        let Some((s, _)) = sub(g) else { return false };
                        fields.tail = Some(s);
                    }
                    continue;
                }
                if g == 0 {
                    continue;
                }
                let Some((s, e)) = sub(g) else { return false };
                let text = &line[s..e];
                if !parse_field(CONV[i], text, fields) {
                    return false;
                }
            }
            true
        }

        fn line_nomatch(&mut self, line: &str, fields: &mut Fields) {
            fields.namebuf.clear();
            fields.lnum = 0;
            fields.valid = false;
            fields.errmsg = line.to_string();
        }

        /// `%D` (enter) / `%X` (leave) directory stack maintenance.
        fn parse_dir_pfx(&mut self, idx: u8, fields: &Fields) -> Result<(), ()> {
            if idx == b'D' {
                if fields.namebuf.is_empty() {
                    return Err(()); // E379: missing directory name
                }
                self.directory = Some(push_dir(&fields.namebuf, &mut self.dir_stack));
            } else {
                self.directory = pop_dir(&mut self.dir_stack);
            }
            Ok(())
        }

        /// `%O`/`%P`/`%Q` global-file prefixes. Returns the line tail to re-scan
        /// when there's trailing content (vim's `QF_MULTISCAN`).
        fn parse_file_pfx(&mut self, idx: u8, fields: &mut Fields, line: &str) -> Option<String> {
            // The named file's existence isn't checked (no fs in the core); treat
            // every `%O`/`%P`/`%Q` name as present.
            if idx == b'P' && !fields.namebuf.is_empty() {
                self.currfile = Some(push_dir(&fields.namebuf, &mut self.file_stack));
            } else if idx == b'Q' {
                self.currfile = pop_dir(&mut self.file_stack);
            }
            fields.namebuf.clear();
            if let Some(off) = fields.tail {
                let rest = line[off..].trim_start().to_string();
                if !rest.is_empty() {
                    self.multiscan = true;
                    return Some(rest);
                }
            }
            None
        }

        /// `%C`/`%Z` continuation: fold this line's data into the previous entry.
        fn parse_multiline_pfx(&mut self, idx: u8, fields: &Fields) {
            if !self.multiignore {
                // Resolve before the mutable borrow of `entries` below.
                let resolved = self.resolve_fname(fields);
                if let Some(prev) = self.entries.last_mut() {
                    if !fields.errmsg.is_empty() {
                        prev.text.push('\n');
                        prev.text.push_str(&fields.errmsg);
                    }
                    if prev.nr == -1 {
                        prev.nr = fields.enr;
                    }
                    if fields.typ.is_ascii_graphic() && prev.typ == 0 {
                        prev.typ = fields.typ;
                    }
                    if prev.lnum == 0 {
                        prev.lnum = fields.lnum;
                    }
                    if prev.end_lnum == 0 {
                        prev.end_lnum = fields.end_lnum;
                    }
                    if prev.col == 0 {
                        prev.col = fields.col;
                        prev.vcol = fields.use_viscol;
                    }
                    if prev.end_col == 0 {
                        prev.end_col = fields.end_col;
                    }
                    if prev.filename.is_none() {
                        prev.filename = resolved;
                    }
                }
            }
            if idx == b'Z' {
                self.multiline = false;
                self.multiignore = false;
            }
        }

        /// Resolve the entry's filename against the directory/file stacks, mirroring
        /// `qf_add_entry`'s filename selection + `qf_get_fnum`.
        fn resolve_fname(&self, fields: &Fields) -> Option<String> {
            let raw = if !fields.namebuf.is_empty() || self.directory.is_some() {
                fields.namebuf.as_str()
            } else if fields.valid {
                self.currfile.as_deref().unwrap_or("")
            } else {
                ""
            };
            if raw.is_empty() {
                return None;
            }
            match &self.directory {
                Some(dir) if !Path::new(raw).is_absolute() => Some(format!("{dir}/{raw}")),
                _ => Some(raw.to_string()),
            }
        }

        fn add_entry(&mut self, fields: &Fields) {
            let filename = self.resolve_fname(fields);
            self.entries.push(QfEntry {
                filename,
                bufnr: fields.bnr,
                module: fields.module.clone(),
                lnum: fields.lnum,
                end_lnum: fields.end_lnum,
                col: fields.col,
                end_col: fields.end_col,
                vcol: fields.use_viscol,
                nr: fields.enr,
                pattern: fields.pattern.clone(),
                text: fields.errmsg.clone(),
                typ: fields.typ,
                valid: fields.valid,
            });
        }
    }

    enum LineStatus {
        AddEntry,
        Ignore,
        Rescan(String),
    }

    /// Extract a single numeric/char/pattern field from its matched `text` (vim's
    /// `qf_parse_fmt_*`). Returns `false` to reject the whole match.
    fn parse_field(conv: u8, text: &str, fields: &mut Fields) -> bool {
        match conv {
            b'b' => fields.bnr = atoi(text),
            b'n' => fields.enr = atoi(text),
            b'l' => fields.lnum = atoi(text) as usize,
            b'e' => fields.end_lnum = atoi(text) as usize,
            b'c' => fields.col = atoi(text) as usize,
            b'k' => fields.end_col = atoi(text) as usize,
            b't' => fields.typ = text.bytes().next().unwrap_or(0),
            b'v' => {
                fields.col = atoi(text) as usize;
                fields.use_viscol = true;
            }
            b'p' => {
                // The pointer line's screen column: count chars, expanding tabs to
                // the next multiple of 8.
                let mut col = 0usize;
                for b in text.bytes() {
                    col += 1;
                    if b == b'\t' {
                        col += 7;
                        col -= col % 8;
                    }
                }
                fields.col = col + 1;
                fields.use_viscol = true;
            }
            b's' => {
                // A literal search pattern: `^\V…\$`.
                let mut p = String::from("^\\V");
                p.push_str(text);
                p.push_str("\\$");
                fields.pattern = p;
            }
            b'o' => fields.module.push_str(text),
            _ => {}
        }
        true
    }

    /// Leading-integer parse (vim's `atol`): stops at the first non-digit.
    fn atoi(s: &str) -> i32 {
        let s = s.trim_start();
        let (neg, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let mut n: i64 = 0;
        for b in digits.bytes() {
            if !b.is_ascii_digit() {
                break;
            }
            n = n * 10 + i64::from(b - b'0');
            if n > i64::from(i32::MAX) {
                n = i64::from(i32::MAX);
                break;
            }
        }
        let n = if neg { -n } else { n };
        n as i32
    }

    /// Push `dir` onto the directory/file stack, resolving a relative entry under
    /// the current top (a simplification of vim's `qf_push_dir`). Returns the new
    /// top.
    fn push_dir(dir: &str, stack: &mut Vec<String>) -> String {
        let resolved = match stack.last() {
            Some(top) if !Path::new(dir).is_absolute() => format!("{top}/{dir}"),
            _ => dir.to_string(),
        };
        stack.push(resolved.clone());
        resolved
    }

    /// Pop the top of the stack, returning the new top (vim's `qf_pop_dir`).
    fn pop_dir(stack: &mut Vec<String>) -> Option<String> {
        stack.pop();
        stack.last().cloned()
    }
}
