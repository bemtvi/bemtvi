//! Draining queued side effects to convergence: applying the Lua chunk's
//! highlights/commands/output/panel/LSP/loop/buffer ops, the Rust→Lua buffer
//! mirror, event-loop completions, and the `run_pending` fixpoint.

#[cfg(feature = "native")]
use crate::evloop::{LoopCommand, LoopEvent};
use crate::{EditHost, WindowStatusline};
use bemtvi_core::highlight::HlDef;
use bemtvi_core::{
    command_pending_after, parse_color, parse_keys, BorderStyle, BufferId, CommandContinuation,
    DecorScope, DecorViewport, DeferredCmd, Editor, FloatAnchor, FloatConfig, FloatRelative,
    QfAction, QfEntry, QfWhich, TabId, UndoEntry, UndoTreeView, WindowConfigSpec, WindowId,
};
use bemtvi_lua::FsJob;
use bemtvi_lua::{
    BoMirror, BufBytesEdit, BufMirror, BufOp, CallbackArgs, DecorInvalidate, DecorPublish, DockOp,
    ExtmarkMirror, ExtmarkOp, FloatMirror, GoMirror, HlDefMirror, HlSet, JumpMirror, LayerOp,
    LinesDelta, LoopOp, NamedListOp, OptionValue, PanelOp, QfItem, QfMirror, StatuslineKind,
    StatuslineTarget, TabMirror, TabOp, TsOp, ViewOp, VirtDecorData, WindowMirror, WindowOp,
};
use rmpv::Value;
use std::collections::HashSet;

/// Byte offset of a neovim 0-based `(row, col)` position in `buf`, clamped into
/// the buffer (row into `[0, line_count]`, col into the line's byte length) the
/// way neovim tolerates out-of-range extmark positions. `col` is a byte offset
/// within the line, matching the rest of bemtvi's column model.
/// Lower a core built-in [`CommandContinuation`] into the [`KeyPending`](crate::keymap::KeyPending)
/// wire continuation a which-key renders, mapping `group` to the matching
/// [`ContinuationKind`](crate::keymap::ContinuationKind). The `desc` is always present
/// for a built-in (every enumerated key is documented).
fn builtin_to_continuation(c: &CommandContinuation) -> crate::keymap::Continuation {
    crate::keymap::Continuation {
        key: c.key.clone(),
        desc: Some(c.desc.to_string()),
        kind: if c.group {
            crate::keymap::ContinuationKind::Group
        } else {
            crate::keymap::ContinuationKind::Map
        },
        // A built-in continuation is always reachable in the state that surfaced it.
        available: true,
    }
}

/// Merge built-in continuations into a withheld mapped-prefix context, then re-sort
/// by key so the payload stays deterministic (the source-A list was already sorted;
/// the union must be too). A built-in key the user has *also* mapped is dropped — the
/// mapped entry wins, since that is what actually fires — so e.g. a user `gd` keeps
/// its own `desc` rather than gaining a duplicate built-in row.
fn merge_builtin_continuations(
    kp: &mut crate::keymap::KeyPending,
    builtin: &[CommandContinuation],
) {
    let have: HashSet<String> = kp.continuations.iter().map(|c| c.key.clone()).collect();
    let mut added: Vec<crate::keymap::Continuation> = builtin
        .iter()
        .filter(|c| !have.contains(&c.key))
        .map(builtin_to_continuation)
        .collect();
    if added.is_empty() {
        return;
    }
    kp.continuations.append(&mut added);
    kp.continuations.sort_by(|a, b| a.key.cmp(&b.key));
}

/// Convert a Lua-side [`QfItem`] (from `setqflist`) into a core [`QfEntry`]. The
/// type char is the first byte of the (possibly empty) `type` string.
/// The core scope one queued [`DecorInvalidate`] selects. A `win` is the narrowest
/// ask and wins over a `buf` (the Lua wrapper rejects passing both, so this only
/// settles the wire form); neither ⇒ every visible window.
fn decor_scope(req: DecorInvalidate) -> DecorScope {
    match (req.win, req.buf) {
        (Some(win), _) => DecorScope::Window(WindowId(win)),
        (None, Some(buf)) => DecorScope::Buffer(BufferId(buf)),
        (None, None) => DecorScope::All,
    }
}

fn qf_entry_from_item(it: QfItem) -> QfEntry {
    QfEntry {
        filename: it.filename,
        bufnr: it.bufnr,
        module: it.module,
        lnum: it.lnum,
        end_lnum: it.end_lnum,
        col: it.col,
        end_col: it.end_col,
        vcol: it.vcol,
        nr: it.nr,
        pattern: it.pattern,
        text: it.text,
        typ: it.typ.bytes().next().unwrap_or(0),
        valid: it.valid,
    }
}

/// Map an `btv.complete` accept-behavior string to the core enum. `"insert"` keeps a
/// word suffix past the cursor; anything else (`"replace"`, or an empty/unknown
/// string) is the `Replace` default — swap the whole word. The Lua wrapper validates
/// the surface, so this only ever sees the two names in practice.
fn parse_accept_behavior(s: &str) -> bemtvi_core::AcceptBehavior {
    match s {
        "insert" => bemtvi_core::AcceptBehavior::Insert,
        _ => bemtvi_core::AcceptBehavior::Replace,
    }
}

/// Project a core [`QfList`]'s entries into the [`QfMirror`] rows the Lua side
/// reads (`btv._qflist` / `btv._loclist[win]`).
fn qf_mirror_items(list: &bemtvi_core::QfList) -> Vec<QfMirror> {
    list.items
        .iter()
        .map(|e| QfMirror {
            filename: e.filename.clone().unwrap_or_default(),
            bufnr: e.bufnr,
            module: e.module.clone(),
            lnum: e.lnum as i64,
            end_lnum: e.end_lnum as i64,
            col: e.col as i64,
            end_col: e.end_col as i64,
            vcol: e.vcol,
            nr: e.nr,
            pattern: e.pattern.clone(),
            text: e.text.clone(),
            typ: if e.typ == 0 {
                String::new()
            } else {
                (e.typ as char).to_string()
            },
            valid: e.valid,
        })
        .collect()
}

/// Resolve the Lua-bridge [`VirtDecorData`] (string-tagged positions/modes) into
/// the typed `bemtvi_core::VirtDecor` the editor stores. `virt_text_win_col` wins
/// over `virt_text_pos` (matching neovim, where a fixed column overrides the
/// relative placement). The position / hl-mode strings were validated loud at the
/// scripting boundary, so an unknown value here falls back to the neovim default.
fn virt_decor_to_core(d: VirtDecorData) -> bemtvi_core::VirtDecor {
    use bemtvi_core::{HlMode, VirtChunk, VirtTextPos};
    let chunks = |cs: Vec<bemtvi_lua::VirtChunkData>| -> Vec<VirtChunk> {
        cs.into_iter()
            .map(|c| VirtChunk {
                text: c.text,
                hl_group: c.hl_group,
            })
            .collect()
    };
    let virt_text_pos = if let Some(col) = d.virt_text_win_col {
        VirtTextPos::WinCol(col.max(0) as u16)
    } else {
        match d.virt_text_pos.as_deref() {
            Some("inline") => VirtTextPos::Inline,
            Some("overlay") => VirtTextPos::Overlay,
            Some("right_align") => VirtTextPos::RightAlign,
            _ => VirtTextPos::Eol,
        }
    };
    let hl_mode = match d.hl_mode.as_deref() {
        Some("combine") => HlMode::Combine,
        Some("blend") => HlMode::Blend,
        _ => HlMode::Replace,
    };
    bemtvi_core::VirtDecor {
        virt_text: chunks(d.virt_text),
        virt_text_pos,
        virt_text_hide: d.virt_text_hide,
        virt_text_fg_only: d.virt_text_fg_only,
        hl_mode,
        virt_lines: d.virt_lines.into_iter().map(chunks).collect(),
        virt_lines_above: d.virt_lines_above,
        sign_text: d.sign_text,
        sign_hl_group: d.sign_hl_group,
        line_fill: d.line_fill.map(|c| VirtChunk {
            text: c.text,
            hl_group: c.hl_group,
        }),
        line_hl_group: d.line_hl_group,
    }
}

fn byte_of(buf: &bemtvi_core::Buffer, row: i64, col: i64) -> usize {
    let n = buf.line_count();
    let row = (row.max(0) as usize).min(n);
    // `line_len`, not `line(...).len()`: the latter copies the whole line into a
    // fresh String just to read its length, and this runs per extmark set — e.g.
    // once per mark of every `btv.decor` viewport publish (per scroll frame).
    let line_len = if row < n { buf.line_len(row) } else { 0 };
    let col = (col.max(0) as usize).min(line_len);
    buf.line_start(row) + col
}

/// neovim 0-based `(row, col)` of byte offset `byte` in `buf` — the inverse of
/// [`byte_of`], for projecting stored extmark anchors back into the Lua mirror.
/// `col` is a byte offset within the line.
fn byte_rowcol(buf: &bemtvi_core::Buffer, byte: usize) -> (u64, u64) {
    let byte = byte.min(buf.len_bytes());
    let row = buf.byte_to_line(byte);
    let col = byte - buf.line_start(row);
    (row as u64, col as u64)
}

/// Whether namespace `ns` is internal editor state (multi-cursor heads and their
/// visual anchors, snippet tabstops, the `:s` preview, diagnostics) rather than a
/// user-visible extmark — those are kept out of the `nvim_buf_get_extmarks` mirror.
fn is_reserved_ns(ns: u32) -> bool {
    ns == bemtvi_core::extmark::CURSOR_NS
        || ns == bemtvi_core::extmark::ANCHOR_NS
        || ns == bemtvi_core::extmark::SNIPPET_NS
        || ns == bemtvi_core::extmark::SUBST_PREVIEW_NS
        || ns == bemtvi_core::extmark::DIAGNOSTIC_NS
}

/// Which marks an edit can have moved *in `(row, col)` terms*.
///
/// The distinction that makes the refresh cheap: shifting a mark's byte anchor is not
/// the same as changing its row/column. Typing a character on one line slides the
/// anchors of every later mark, but their `(row, col)` is **identical** — only marks
/// on the edited line move. So a same-line edit needs to refresh a handful of marks,
/// not all of them, and each refresh that *is* needed costs two rope lookups
/// (`byte_rowcol`) which is what made the naive per-keystroke rebuild expensive.
#[derive(Clone, Copy)]
enum PosScope {
    /// Refresh every mark — a structural change, a resync, or a batch we can't bound.
    All,
    /// Refresh only marks touching the byte range `[lo, hi)` (`hi = None` ⇒ unbounded,
    /// which is the case when the edit changed the line count and every later mark's
    /// row therefore shifts).
    Window { lo: usize, hi: Option<usize> },
}

impl PosScope {
    /// Whether `m` falls in this scope. A range mark counts if *either* edge does — a
    /// mark starting on an earlier line but ending inside the edited one has its
    /// `end_row`/`end_col` changed even though its start is untouched.
    fn touches(self, m: &bemtvi_core::extmark::Extmark) -> bool {
        let (lo, hi) = match self {
            PosScope::All => return true,
            PosScope::Window { lo, hi } => (lo, hi),
        };
        let in_range = |p: usize| p >= lo && hi.is_none_or(|h| p < h);
        in_range(m.start) || m.end.is_some_and(in_range)
    }
}

/// The byte window an edit batch can have moved marks within, for [`PosScope`].
///
/// Only a **single-edit** batch is bounded, and that restriction is load-bearing
/// rather than conservatism. Each edit's byte offsets are expressed in the buffer as
/// it stood before *that* edit, so across a batch they live in different coordinate
/// spaces: an earlier edit's `new_end_byte` can point past the final rope (resolving
/// it panics), and — the quieter bug — a batch whose row changes *cancel out* to zero
/// still moved every mark between the two edits, so bounding on the net delta would
/// silently stale them. Folding that soundly needs the same forward/non-overlapping
/// mapping [`fold_mirror_edits`] does; until something shows multi-edit batches are
/// hot, refreshing all of a buffer's marks is the correct answer for them.
///
/// For the single edit that the typing path actually produces: `lo` is the start of
/// its line (a mark anywhere on that line can have its column changed), and `hi`
/// closes at the next line's start when the edit did not change the line count —
/// every mark beyond keeps both row and column. An edit that adds or removes lines
/// shifts every later row, so the window stays open.
///
/// `None` ⇒ refresh everything.
fn extmark_dirty_window(
    buf: &bemtvi_core::Buffer,
    batch: &bemtvi_core::EditBatch,
) -> Option<PosScope> {
    if batch.resync {
        return None;
    }
    let [edit] = &batch.edits[..] else {
        return None;
    };
    // Nothing ahead of an edit's start moves, so this offset means the same thing
    // before and after it — but clamp anyway rather than trust it against the rope.
    let lo_byte = edit.start_byte.min(buf.len_bytes());
    let lo = buf.line_start(buf.byte_to_line(lo_byte));
    let hi = if edit.new_end_point.0 == edit.old_end_point.0 {
        let last = edit.new_end_byte.min(buf.len_bytes());
        let row = buf.byte_to_line(last);
        // Start of the following line, or unbounded if the edit touched the last one.
        (row + 1 < buf.line_count()).then(|| buf.line_start(row + 1))
    } else {
        None
    };
    Some(PosScope::Window { lo, hi })
}

/// Every mirrored mark's *positions* in `buf`, flat:
/// `[ns, id, row, col, end_row, end_col]` per mark, with `-1, -1` for a mark with no
/// end. This is the whole payload an edit needs — `ExtmarkStore::shift` moves byte
/// anchors and nothing else, so a mark's decorations (`hl_group`, priority, the sign
/// / line-fill / line-hl payloads, gravity) are re-pushed only when the store's
/// structural generation moves. One flat integer array per buffer costs no per-mark
/// table or string allocation, which is what made the old per-keystroke rebuild
/// O(marks) with a punishing constant. See
/// `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
fn extmark_positions(buf: &bemtvi_core::Buffer, scope: PosScope) -> Vec<i64> {
    let mut flat = Vec::new();
    for (ns, m) in buf.extmarks.iter_with_ns() {
        if is_reserved_ns(ns) {
            continue;
        }
        if !scope.touches(m) {
            continue;
        }
        let (row, col) = byte_rowcol(buf, m.start);
        let (end_row, end_col) = match m.end {
            Some(e) => {
                let (r, c) = byte_rowcol(buf, e);
                (r as i64, c as i64)
            }
            None => (-1, -1),
        };
        flat.extend_from_slice(&[
            ns as i64,
            m.id as i64,
            row as i64,
            col as i64,
            end_row,
            end_col,
        ]);
    }
    flat
}

/// Fold a mirror edit batch into one replaced **row** span
/// `(start, old_end_row, new_end_row)`. `start` and `old_end_row` are rows in the
/// buffer as it stood *before* the batch; `new_end_row` is a row in the buffer as it
/// stands now. All three are edit *positions*, so the rows they touch are inclusive
/// at both ends — the caller converts to an end-exclusive line span.
///
/// Each edit's points are expressed in the buffer as it stood before *that* edit, so
/// folding them requires mapping each one back through the row shift of every
/// preceding edit. That mapping is only sound while the batch moves strictly forward
/// without overlapping — which is the shape of every common batch (a multi-key
/// insert, a `:s` walking down the buffer, the trailing-newline `normalize`). For
/// anything else (out-of-order or overlapping edits) this returns `None` and the
/// caller pushes the buffer in full rather than guessing at a span.
fn fold_mirror_edits(edits: &[bemtvi_core::BufferEdit]) -> Option<(usize, usize, usize)> {
    bemtvi_core::buffer::fold_edit_rows(edits)
}

/// Project a core [`BufferEdit`] (absolute byte offsets + `(row, byte-col)`
/// points) into neovim's `on_bytes` argument tuple, whose row/col fields are
/// *relative* deltas: `start_*` is the absolute start, the `old_*`/`new_*` triples
/// are `(rows spanned, col on the last spanned row, byte count)`. This is the
/// inverse of the vendored `LanguageTree:_on_bytes` reconstruction
/// (`old_end_col = old_col + (old_row == 0 ? start_col : 0)`, etc.), so a round
/// trip through it recovers the original absolute edit.
fn on_bytes_edit(bufnr: u64, tick: u64, e: &bemtvi_core::BufferEdit) -> BufBytesEdit {
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

/// Build a core [`FloatConfig`] from the validated string fields the float bridges
/// carry (`WindowOp::OpenFloat`, `ViewOp::MountFloat`). The prelude already validated the
/// enumerated strings against the supported set, so any unexpected value here is a bug —
/// returned as `Err(msg)` for the caller to echo loudly rather than silently mispositioning.
/// `cur_win` is the parent for `relative == "win"` when the caller passed `win == 0`.
///
/// `width`/`height` are size specs (cells or a `vw`/`vh`/`%` fraction); an empty /
/// unparseable spec is an error (floats require a size). `align` is the high-level
/// alignment keyword (`None` / empty ⇒ the low-level `anchor`/`row`/`col` form);
/// `margin` is the `[top, right, bottom, left]` edge inset for an aligned float.
#[allow(clippy::too_many_arguments)]
fn build_float_config(
    relative: &str,
    win: u64,
    cur_win: WindowId,
    anchor: &str,
    row: i64,
    col: i64,
    width: &str,
    height: &str,
    align: Option<&str>,
    margin: [u64; 4],
    zindex: u32,
    focusable: bool,
    border: &str,
    title: Option<String>,
) -> Result<FloatConfig, String> {
    let relative = match relative {
        "editor" => FloatRelative::Editor,
        "cursor" => FloatRelative::Cursor,
        "win" => FloatRelative::Win(if win == 0 { cur_win } else { WindowId(win) }),
        other => return Err(format!("invalid 'relative': '{other}'")),
    };
    let anchor =
        FloatAnchor::from_keyword(anchor).ok_or_else(|| format!("invalid 'anchor': '{anchor}'"))?;
    let border =
        BorderStyle::from_keyword(border).ok_or_else(|| format!("invalid 'border': '{border}'"))?;
    let width = parse_extent(width).ok_or_else(|| format!("invalid 'width': '{width}'"))?;
    let height = parse_extent(height).ok_or_else(|| format!("invalid 'height': '{height}'"))?;
    let align = parse_align(align)?;
    Ok(FloatConfig {
        relative,
        anchor,
        row: row as isize,
        col: col as isize,
        width,
        height,
        align,
        margin: build_margin(margin),
        zindex,
        focusable,
        border,
        title: normalize_title(title),
        // The cursor-line-avoiding placement is core-owned (the LSP doc popups);
        // a Lua-mounted float takes the plain `anchor`/`row`/`col` geometry.
        flip: false,
    })
}

/// Normalize a float `title`: an empty string means *no title*. The single home
/// of that policy — both decode surfaces (the msgpack `parse_title` and the
/// Lua-op arms below) route through it, so "empty clears" can't drift between
/// them again (the `SetConfig` arm once stored `Some("")` where the msgpack
/// path cleared).
pub(crate) fn normalize_title(title: Option<String>) -> Option<String> {
    title.filter(|t| !t.is_empty())
}

/// Build a core [`Margin`] from the `[top, right, bottom, left]` cell counts the
/// wire carries.
pub(crate) fn build_margin(m: [u64; 4]) -> bemtvi_core::Margin {
    bemtvi_core::Margin {
        top: m[0] as usize,
        right: m[1] as usize,
        bottom: m[2] as usize,
        left: m[3] as usize,
    }
}

/// Parse the high-level alignment keyword into an `Option<Align>`: `None` / `""`
/// ⇒ `None` (the low-level anchor/offset form), a known word ⇒ `Some(_)`, an
/// unknown word ⇒ a loud `Err` (the prelude validated it, so this is a bug guard).
pub(crate) fn parse_align(align: Option<&str>) -> Result<Option<bemtvi_core::Align>, String> {
    match align {
        None | Some("") => Ok(None),
        Some(word) => bemtvi_core::Align::from_keyword(word)
            .map(Some)
            .ok_or_else(|| format!("invalid 'align': '{word}'")),
    }
}

/// Translate a core [`FloatConfig`] into the [`FloatMirror`] the `btv._wins`
/// mirror carries — the enums become the strings `nvim_win_get_config` returns,
/// so bemtvi-lua never sees the core's float types. The inverse of the
/// `parse_float_config` / `WindowOp::OpenFloat` parse.
///
/// `width`/`height` are the **resolved** inner cells read off the laid-out window
/// (the float's `Extent` is resolved against the live editor area every layout),
/// so a fractional float reports its true on-screen size — matching neovim's
/// integer `nvim_win_get_config` and self-healing on resize. The caller passes
/// `window_content_size(id)`.
fn float_mirror(cfg: FloatConfig, width: usize, height: usize) -> FloatMirror {
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
        width: width as u64,
        height: height as u64,
        align: cfg.align.map(|a| a.as_str().to_string()),
        zindex: cfg.zindex as u64,
        focusable: cfg.focusable,
        border: cfg.border.as_str().to_string(),
        title: cfg.title,
    }
}

/// Apply one keymap bucket's batch of named widget actions through its core
/// handler (`apply`). An unknown action name fails loud (core returns `Err`)
/// and is surfaced on the message line rather than silently ignored — the one
/// drain loop behind every `btv._*_action` bucket.
fn drain_widget_actions(
    editor: &mut Editor,
    actions: Vec<String>,
    apply: impl Fn(&mut Editor, &str) -> Result<(), String>,
) {
    for action in actions {
        if let Err(e) = apply(editor, &action) {
            editor.echo(format!("E5108: {e}"));
        }
    }
}

impl EditHost {
    /// Apply the side effects the last Lua chunk left in the runtime: highlight
    /// definitions fold into the core registry, queued ex-commands run against
    /// the editor, and the final captured `print` / `nvim_echo` line becomes the
    /// message.
    ///
    /// Wrapped in a core scroll gesture so a *navigation* that runs from Lua slides
    /// like the native one: a keymap RHS such as the built-in `]d`
    /// (`btv.diagnostic.goto_next` → an `LspOp` drained below) never reaches
    /// [`Editor::input`], which is where a typed `G`/`n`/`<C-o>` takes its own
    /// viewport snapshot — so without this the viewport teleports. The core decides
    /// whether to animate (mode, an intervening edit, a buffer switch, the
    /// `'scrollanim'` options, a move of more than one line).
    pub(crate) fn apply_lua_effects(&mut self) {
        self.editor.begin_scroll_gesture();
        let pre_tick = self.editor.buffer().changedtick;
        self.apply_lua_effects_inner();
        let edited = self.editor.buffer().changedtick != pre_tick;
        self.editor.end_scroll_gesture(edited);
    }

    fn apply_lua_effects_inner(&mut self) {
        // Refresh the autocmd cache before running any queued command, so a
        // `btv.cmd('edit …')` (which runs through `editor.command` here, not the
        // `run_command` RPC path) sees an up-to-date `bufreadcmd_active` and defers a
        // directory / `*Cmd`-matched open to the explorer's `BufReadCmd` handler. Cheap:
        // a version check that only rebuilds when the registry changed.
        self.refresh_au_events();
        for hl in self.lua.take_highlights() {
            self.editor.highlights.set_ns(hl.ns, &hl.name, hl_def(&hl));
        }
        for cmd in self.lua.take_commands() {
            self.editor.command(&cmd);
        }
        // `btv._cmdline_set_arg(path)`: the file picker confirm pasting a chosen path
        // into the still-open command line's argument token (no execute — the user
        // runs the filled line with `<CR>`).
        for path in self.lua.take_cmdline_set_args() {
            self.editor.cmdline_replace_arg(&path);
        }
        // Each captured `print` / `nvim_echo` line becomes a message: the last
        // is shown on the message line, and every line lands in `:messages`. Error
        // writers (`btv.err_write*`) route through `echo_err` so they paint red.
        // Under the headless `--lua` one-shot ([`lua_stdio`](EditHost::lua_stdio)) there
        // is no UI, so each line goes to the real stdout (plain) / stderr (error) instead,
        // reaching the shell/CI that launched it.
        for line in self.lua.take_output() {
            match (self.lua_stdio, line.error) {
                (true, false) => println!("{}", line.text),
                (true, true) => eprintln!("{}", line.text),
                (false, false) => self.editor.echo(line.text),
                (false, true) => self.editor.echo_err(line.text),
            }
        }
        // Named widget actions the keymap buckets fired, one batch per surface —
        // picker (`btv._picker_action`), `btv.ui.select` list, focused `btv.view`
        // buffer (`<CR>` confirm), quickfix / loclist display (`<CR>` jump), and
        // the open command line. Each applies through its core handler; an unknown
        // action name fails loud (core returns `Err`) and is surfaced rather than
        // silently ignored.
        let editor = &mut self.editor;
        drain_widget_actions(
            editor,
            self.lua.take_picker_actions(),
            Editor::apply_picker_action,
        );
        drain_widget_actions(
            editor,
            self.lua.take_select_actions(),
            Editor::apply_select_action,
        );
        drain_widget_actions(
            editor,
            self.lua.take_view_actions(),
            Editor::apply_view_action,
        );
        drain_widget_actions(editor, self.lua.take_qf_actions(), Editor::apply_qf_action);
        drain_widget_actions(
            editor,
            self.lua.take_cmdline_actions(),
            Editor::apply_cmdline_action,
        );
        // Helix actions a `helix`-bucket keymap fired (`btv._helix_action`): apply each
        // named verb (with its optional count) to the editor. Unknown names fail loud.
        for (action, count) in self.lua.take_helix_actions() {
            if let Err(e) = self.editor.apply_helix_action(&action, count) {
                self.editor.echo(format!("E5108: {e}"));
            }
        }
        // Dock requests from `btv.dock.*` drive the core's dock (edge-panel) state.
        for op in self.lua.take_dock_ops() {
            match op {
                DockOp::Open { side, size, buf } => self.editor.open_dock_named(
                    &side,
                    size.map(|s| s as usize),
                    buf.map(bemtvi_core::BufferId),
                ),
                DockOp::Close { side } => self.editor.close_dock_named(&side),
                DockOp::Focus { side } => self.editor.focus_dock_named(&side),
                DockOp::Toggle { side } => self.editor.toggle_dock_named(&side),
                DockOp::Hide { side } => self.editor.hide_dock_named(&side),
                DockOp::Show { side } => self.editor.show_dock_named(&side),
                DockOp::SetOption { side, name, value } => match value {
                    OptionValue::Number(n) => self.editor.set_dock_option_num(&side, &name, n),
                    OptionValue::String(s) => self.editor.set_dock_option_str(&side, &name, s),
                    OptionValue::Bool(b) => {
                        // `autohide` is boolean; route every bool through the numeric
                        // setter (0/1) so it (and any unknown name) is handled there.
                        self.editor.set_dock_option_num(&side, &name, i64::from(b))
                    }
                },
            }
        }
        // Client-directed session swaps from `btv.session.reconnect(spec)`. Unlike the
        // other buckets, these do not touch the editor: each is pushed OUT to the client
        // as a `btv_session_reconnect` notification (the client owns the window + transport
        // and tears down / rebuilds the session, keeping the window). The spec rides
        // verbatim as the single notification param. A plugin can thus initiate the reload
        // from inside the running VM (§B). Sent AS the chunk drains — the client dropping
        // the old transport is what winds this server down, so no loop-break is needed here.
        for spec in self.lua.take_session_reconnects() {
            self.fx.notify("btv_session_reconnect", vec![spec]);
        }
        // Fallback connects from `:connect <url>` with no matching provider (§C): pushed OUT
        // as a `btv_connect_fallback` notification so the client dials the URL with its
        // built-in direct connect (QUIC URI / ssh host). The URL rides verbatim as the single
        // param; like the swap above, no editor state changes here.
        for url in self.lua.take_connect_fallbacks() {
            self.fx.notify("btv_connect_fallback", vec![url]);
        }
        // `btv.view` requests from the view handle methods drive the core's view
        // registry (plugin-owned, read-only content surfaces). Drained *before* the
        // layer crosses below so a `v:mount{...}` (which focuses the view) followed by
        // a `btv.layer.main()` in the same chunk lands focus back in the main area — the
        // file-tree "mount, then return focus to the editor" idiom.
        for op in self.lua.take_view_ops() {
            match op {
                ViewOp::Create {
                    id,
                    name,
                    filetype,
                    namespace,
                    persist,
                } => {
                    // A non-empty `persist` opts the view into cross-session restore, keyed
                    // by `(namespace, persist)`; empty ⇒ ephemeral (the pair is `None`).
                    let persist = (!persist.is_empty()).then_some((namespace, persist));
                    self.editor.create_view(id, name, filetype, persist);
                    // Install the view's buffer-local `<CR>` → on_select map now, off
                    // the synchronously-known backing bufnr (the view is read-only and
                    // may never be the current buffer for a `FileType` event, so it
                    // installs at create rather than via a FileType autocmd).
                    if let Some(buf) = self.editor.view_buffer(id) {
                        if let Err(e) = self.lua.install_view_keymaps(buf.0) {
                            self.editor.echo(format!("E5108: {e}"));
                        }
                    }
                }
                ViewOp::SetLines { id, lines } => self.editor.set_view_lines(id, lines),
                ViewOp::SetCursor { id, line } => self.editor.set_view_cursor(id, line as usize),
                ViewOp::MountDock { id, side, size } => {
                    self.editor
                        .mount_view_dock(id, &side, size.map(|s| s as usize))
                }
                ViewOp::MountSplit { id, vertical } => self.editor.mount_view_split(id, vertical),
                ViewOp::MountTab { id } => self.editor.mount_view_tab(id),
                ViewOp::MountFloat {
                    id,
                    relative,
                    win,
                    anchor,
                    row,
                    col,
                    width,
                    height,
                    align,
                    margin,
                    zindex,
                    focusable,
                    border,
                    title,
                    grab,
                } => {
                    let cur_win = self.editor.current_window_id();
                    match build_float_config(
                        &relative,
                        win,
                        cur_win,
                        &anchor,
                        row,
                        col,
                        &width,
                        &height,
                        align.as_deref(),
                        margin,
                        zindex,
                        focusable,
                        &border,
                        title,
                    ) {
                        Ok(config) => self.editor.mount_view_float(id, config, grab),
                        Err(e) => self.editor.echo(format!("btv.view:mount{{ float }}: {e}")),
                    }
                }
                ViewOp::Adopt { id, win } => self.editor.adopt_view(id, WindowId(win)),
                ViewOp::Unmount { id } => self.editor.unmount_view(id),
                ViewOp::Focus { id } => self.editor.focus_view(id),
                ViewOp::Destroy { id } => self.editor.destroy_view(id),
                ViewOp::CollapseUnclaimed => self.editor.collapse_unclaimed_view_restores(),
            }
        }
        // `btv.panel` open / close — mount a scripted panel (a `nomodifiable` buffer in a
        // focus-locked bottom overlay) or dismiss the open one. Behavior inside it rides
        // the buffer's `FileType` ftplugin, so nothing else crosses the bridge.
        for op in self.lua.take_panel_ops() {
            match op {
                PanelOp::Open {
                    name,
                    lines,
                    filetype,
                    height,
                    margin,
                } => {
                    // An empty / absent height ⇒ `None` (the default listing height);
                    // a present-but-unparseable spec is a loud echo, then default.
                    let height = match height.as_deref() {
                        None | Some("") => None,
                        Some(spec) => match parse_extent(spec) {
                            Some(e) => Some(e),
                            None => {
                                self.editor
                                    .echo(format!("btv.panel.open: invalid 'height': '{spec}'"));
                                None
                            }
                        },
                    };
                    self.editor.open_script_panel(
                        name,
                        lines,
                        filetype,
                        height,
                        build_margin(margin),
                    )
                }
                PanelOp::Close => self.editor.close_panel(),
            }
        }
        // Layer crosses from `btv.open` / `btv.layer.*` drive the core's layer machine
        // (the main editor area + each open dock).
        for op in self.lua.take_layer_ops() {
            match op {
                LayerOp::Open { path, where_main } => {
                    self.editor.open_path_in_layer(&path, where_main)
                }
                LayerOp::Focus { target } => self.editor.focus_layer_named(&target),
            }
        }
        // Terminal-open requests from `btv.terminal.open` open a terminal job in the
        // current window; the core enqueues the PTY spawn, drained at the end of this
        // convergence by `take_pending_terminal` → `dispatch_terminal_ops`.
        for req in self.lua.take_terminal_open_reqs() {
            self.editor.open_terminal(req.argv, req.cwd);
        }
        // Server-start requests from `vim.lsp.start` (the `vim.lsp.enable` FileType
        // dispatcher) bind a buffer to its language server and ensure it is spawned.
        // The consumer (`apply_lsp_op`) is shared: native runs servers through the
        // async `LspManager`, wasm through the `SyncLspClient` over the daemon wire
        // (Phase 6e). A serverless browser session (no daemon) has no process host, so
        // `apply_lsp_op`'s server-start path fails *loud* there rather than silently
        // dropping the request — see its `has_remote_lsp` guard.
        for op in self.lua.take_lsp_ops() {
            self.apply_lsp_op(op);
        }
        // Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `btv.run` /
        // `btv.timer` / async `vim.system`: a `Schedule` is serviced directly (queued
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
        // `btv._extmarks` mirror.
        for op in self.lua.take_extmark_ops() {
            self.apply_extmark_op(op);
        }
        // btv.decor publishes: marks a viewport provider produced for a window's
        // visible range (Phase 3). Generation-gated and lowered into the provider's
        // namespace in the extmark layer (drained here so an async provider that
        // publishes from a later off-tick round still lands).
        for publish in self.lua.take_decor_publishes() {
            self.apply_decor_publish(publish);
        }
        // btv.decor.invalidate: "the data my provider draws from changed" — a
        // re-dispatch the viewport signal can't produce on its own, because nothing it
        // watches (buffer, top, bot, changedtick) moved. Lowered onto the core, which
        // drops the affected windows' cached viewport keys so `run_pending`'s
        // recompute re-queues them with a fresh generation; the run this supersedes has
        // any in-flight publish dropped by the ordinary staleness check.
        for req in self.lua.take_decor_invalidations() {
            self.editor.invalidate_decor(decor_scope(req));
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
                // `:set statusline=…` ex path writes. The Lua bridge forwards only
                // canonical names, so ignore the handled? result the `:set` path uses.
                OptionValue::String(s) => {
                    let _ = self.editor.set_global_option_str(&op.name, &s);
                }
            }
        }
        // Per-workspace option overrides from `btv.wso`: set (`Some`) or clear (`None`) the
        // override in the editor's workspace overlay (which wins over the global value). An
        // invalid name / kind echoes the core's error (fail loud — the prelude restricts the
        // surface to global options, so this catches a stray non-global / bad value).
        for op in self.lua.take_workspace_option_ops() {
            use bemtvi_core::options::OptionScalar;
            let value = op.value.map(|v| match v {
                OptionValue::Bool(b) => OptionScalar::Bool(b),
                OptionValue::Number(n) => OptionScalar::Num(n),
                OptionValue::String(s) => OptionScalar::Str(s),
            });
            if let Err(msg) = self.editor.set_workspace_option(&op.name, value) {
                self.editor.echo(msg);
            }
        }
        // Treesitter bridges from `btv.treesitter`: the query-override push
        // (`btv.treesitter.set_query`) and the off-buffer snippet highlighter
        // (`btv.treesitter.highlight`). `SetQuery` mutates the in-process engine — native
        // only (the browser highlights JS-side), so it fails loud on wasm; `Highlight`
        // runs the editor's off-buffer highlighter, which exists in *both* builds (it
        // just returns no spans on the wasm JS-side engine), so it settles its promise
        // everywhere and never fails loud.
        for op in self.lua.take_ts_ops() {
            match op {
                TsOp::Highlight { lang, text, cb_id } => {
                    // Normalize to the engine's phantom-line invariant (a trailing
                    // newline: the last line is treated as phantom) and count the content
                    // lines, then run the same stateless highlighter the preview uses
                    // (injections included) and settle the promise with the spans.
                    let text = if text.ends_with('\n') {
                        text
                    } else {
                        format!("{text}\n")
                    };
                    let nlines = text.matches('\n').count();
                    self.settle_ts_highlight(lang, text, nlines, cb_id);
                }
                #[cfg(feature = "native")]
                TsOp::SetQuery { lang, name, text } => {
                    // `btv.treesitter.set_query`: install the override on the engine
                    // directly — no Lua merge/resolution. A compile failure echoes
                    // loud via `set_ts_query` itself. A query change is rare (config
                    // time) and lang-wide, so drop every buffer's highlight memo
                    // rather than track which are this language; they all re-query on
                    // the next redraw.
                    self.editor.set_ts_query(&lang, &name, text);
                    self.syntax_states.clear();
                }
                #[cfg(not(feature = "native"))]
                TsOp::SetQuery { .. } => {
                    self.editor.echo(
                        "E: btv.treesitter.set_query is not available in the browser build yet",
                    );
                }
                // Unlike `SetQuery`, this needs no cfg split and never fails loud: it
                // is config for the *fragment* highlighter, and the wasm JS-side
                // engine does no off-buffer highlighting at all, so it has no ladder
                // to configure. The prelude ships defaults for several languages, so a
                // loud arm here would greet every browser session with a wall of
                // errors about a surface that isn't there.
                TsOp::SetFragmentContext { lang, templates } => {
                    self.editor.set_ts_fragment_context(&lang, templates);
                }
            }
        }
        // User tree-sitter text-object bindings from `btv.textobject.map`: applied to
        // the editor's text-object registry. Plain editor state (not engine state),
        // so — unlike `ts_ops` above — it applies in every build, native or wasm.
        for op in self.lua.take_textobject_ops() {
            self.editor.set_textobject_map(&op.lhs, op.capture);
        }
        // Register writes from `vim.fn.setreg`: applied to the editor's register
        // file after the chunk — the same store yanks/deletes write. The Lua side
        // already rejected read-only specials and resolved uppercase/`a` append.
        for op in self.lua.take_reg_ops() {
            self.editor
                .set_register_api(op.name, op.text, op.linewise, op.append);
        }
        // Clipboard seeds from `btv.test.clipboard.seed` (plugin-test seam): write the
        // editor's clipboard provider as if an external app set `"+` / `"*`.
        for (text, linewise) in self.lua.take_clipboard_seeds() {
            self.editor.clipboard_seed(&text, linewise);
        }
        // `setqflist` writes: structured items, or raw lines parsed against `efm`
        // (the editor's `'errorformat'` when the op omits one). A malformed efm
        // fails loud on the message line rather than silently dropping the call.
        for op in self.lua.take_qf_ops() {
            // "Send/add these results to a list" (`btv.qf.{send,add}_to_{loc,qf}list`):
            // route the structured items through `list_send`, which honors `'qfdock'`
            // (a dock tab vs a split). `loclist_win == None` targets the global
            // quickfix list, `Some(_)` a location list; the action char picks
            // send (new) vs add (append). Distinct from the `which`-targeted writes.
            if op.send {
                let items = op.items.unwrap_or_default();
                let entries = items.into_iter().map(qf_entry_from_item).collect();
                let action = if op.action == 'a' {
                    QfAction::Add
                } else {
                    QfAction::New
                };
                let to_qf = op.loclist_win.is_none();
                self.editor
                    .list_send(entries, op.title.unwrap_or_default(), action, to_qf);
                continue;
            }
            let action = match op.action {
                'a' => QfAction::Add,
                'r' => QfAction::Replace,
                _ => QfAction::New,
            };
            // A named target (`btv.qf.list(name, …)`) takes precedence over the
            // quickfix / loclist routing: intern the name to its id and write that
            // window-independent list. Otherwise: quickfix list (`loclist_win == None`)
            // vs a window's location list. A `Some(0)` targets the current window
            // (vim's `winnr` 0); any other id is a window handle. Drop the op on a
            // stale window id rather than silently writing the quickfix list.
            let which = if let Some(name) = &op.named {
                Some(QfWhich::Named(self.editor.named_list_id(name)))
            } else {
                match op.loclist_win {
                    None => Some(QfWhich::Quickfix),
                    Some(0) => Some(QfWhich::Location(self.editor.current_window_id())),
                    Some(id) => {
                        let win = WindowId(id);
                        if self.editor.window_ids().contains(&win) {
                            Some(QfWhich::Location(win))
                        } else {
                            self.editor
                                .echo(format!("E957: Invalid window number {id}"));
                            None
                        }
                    }
                }
            };
            let Some(which) = which else { continue };
            let mut ok = true;
            if let Some(items) = op.items {
                let entries = items.into_iter().map(qf_entry_from_item).collect();
                self.editor.qf_set_items(which, entries, action, op.title);
            } else if let Some(lines) = op.lines {
                let efm = op
                    .efm
                    .unwrap_or_else(|| self.editor.global_options().errorformat);
                if let Err(e) = self
                    .editor
                    .qf_set_from_lines(which, &lines, &efm, action, op.title)
                {
                    self.editor.echo(e);
                    ok = false;
                }
            } else {
                // Neither items nor lines: an explicit clear.
                self.editor
                    .qf_set_items(which, Vec::new(), action, op.title);
            }
            // The `:make`/`:grep` post-populate behavior: open the window iff there
            // are entries, then jump to the first valid one. Skipped when the parse
            // failed (the list is unchanged).
            if ok && (op.open || op.goto_first) {
                self.editor.qf_post_populate(which, op.open, op.goto_first);
            }
        }
        // Named-list lifecycle ops, drained *after* the `QfSetOp`s above so a
        // `btv.qf.show` observes the refresh queued just before it (server-sequenced,
        // no `set_current` + `on_next_tick` dance).
        for op in self.lua.take_named_list_ops() {
            match op {
                NamedListOp::Show(name) => self.editor.named_list_show(&name),
                NamedListOp::Drop(name) => self.editor.named_list_drop(&name),
            }
        }
        // `vim.ui.input` prompts (Phase 8): open the editor's command line as a
        // labelled text prompt and remember which callback awaits the result. Only
        // one prompt can be open at a time (a single command line); if several were
        // queued, the last wins (its label/default is what shows) — a documented
        // single-prompt limitation, not a silent drop.
        for req in self.lua.take_ui_inputs() {
            self.editor.open_prompt(
                req.prompt,
                req.default,
                req.history,
                req.complete,
                req.complete_docs,
            );
            self.pending_ui_input = Some(req.cb_id);
        }
        // Prompt completion candidates resolved by an `btv.ui.input{ complete = fn }`
        // source (sync, or a tick after the request for an async source): rebuild the
        // prompt wildmenu from each. Drained every pass so async results land off the
        // keypress that requested them.
        for cands in self.lua.take_prompt_complete_results() {
            let cands: Vec<bemtvi_core::CmdlineCandidate> = cands
                .into_iter()
                .map(|(label, insert, doc, range)| {
                    (label, insert, (!doc.is_empty()).then_some(doc), range)
                })
                .collect();
            self.editor.open_prompt_complete_menu(cands);
        }
        // `btv.ui.select`: open the floating selectable-list widget and remember
        // which callback awaits the chosen index. The Lua wrapper never queues an
        // empty list (it resolves to cancel itself), so the menu always has rows;
        // like the prompt, only one is open at a time (the last queued wins).
        for req in self.lua.take_ui_selects() {
            self.editor
                .open_menu(req.items, bemtvi_core::MenuPlacement::Cursor, 0);
            self.pending_ui_select = Some(req.cb_id);
        }
        // `btv.complete.choice`: a non-grabbing cursor dropdown over a byte range —
        // accepting a row splices the pick over the range natively (no callback), so a
        // plugin snippet engine's own `on_bytes` reacts. The last queued wins.
        for req in self.lua.take_choice_menus() {
            self.editor
                .open_choice_menu(req.sr, req.sc, req.er, req.ec, req.items);
        }
        // `btv.ui.float`: open / update / close the list-less content float. A
        // transient float (`id == 0`) is fire-and-forget and dismissed by the next
        // key; a persistent one (`id != 0`) survives keystrokes until its handle
        // closes it. The last queued op wins. The border keyword is parsed loud
        // here (no silent fallback) — an unknown one echoes and skips the float.
        for req in self.lua.take_ui_floats() {
            if req.close {
                self.editor.close_content_float_id(req.id);
                continue;
            }
            let Some(border) = bemtvi_core::BorderStyle::from_keyword(&req.border) else {
                self.editor
                    .echo(format!("btv.ui.float: unknown border '{}'", req.border));
                continue;
            };
            let placement = match req.relative.as_str() {
                "cursor" => bemtvi_core::MenuPlacement::Cursor,
                "editor" => bemtvi_core::MenuPlacement::Editor,
                "bottom" => bemtvi_core::MenuPlacement::Bottom,
                other => {
                    self.editor
                        .echo(format!("btv.ui.float: unknown relative '{other}'"));
                    continue;
                }
            };
            // Lower each chunk line (`VirtChunkData`) into core's `VirtChunk`, the
            // same chunk type `virt_lines` / `virt_text` use, so the float renders
            // styled spans. A plain caller's single unstyled chunk resolves to
            // normal colors. `id == 0` is transient, non-zero a persistent handle.
            let lines: Vec<Vec<bemtvi_core::VirtChunk>> = req
                .lines
                .into_iter()
                .map(|line| {
                    line.into_iter()
                        .map(|c| bemtvi_core::VirtChunk {
                            text: c.text,
                            hl_group: c.hl_group,
                        })
                        .collect()
                })
                .collect();
            self.editor
                .open_styled_float(lines, req.title, border, placement, req.id);
        }
        // `btv.picker.open`: open the centered fuzzy-finder widget and kick the
        // source's initial run (generation 0, empty query). The source streams
        // candidates back as `picker_pushes` (drained just below) — synchronously
        // for an in-memory source like `buffers`, or later via `on_stdout` for a
        // process source. The chosen item / cancel comes back on `menu_results`,
        // routed to the picker by `picker_active` (a picker and a `ui.select` are
        // the same widget, mutually exclusive).
        // `btv.picker.scorer(src|nil)`: compile the re-ranker into the sandbox now,
        // so a bad expression is reported where it was configured rather than
        // silently at the next picker.
        if let Some(src) = self.lua.take_complete_scorer() {
            self.editor.set_complete_scorer(src);
        }
        if let Some(src) = self.lua.take_picker_scorer() {
            self.editor.set_picker_scorer(src);
        }
        // `btv.fold.text(src|nil)`: the `'foldtext'` expression, compiled now so a
        // bad one is reported where it was configured.
        if let Some(src) = self.lua.take_fold_text() {
            self.editor.set_fold_text(src);
        }
        // `btv.filetype.detect` / `btv.indent.expr`: same compile-at-configure-time
        // contract, so a bad expression is reported where it was written.
        if let Some(src) = self.lua.take_filetype_detect() {
            self.editor.set_filetype_detect(src);
        }
        if let Some(src) = self.lua.take_indent_expr() {
            self.editor.set_indent_expr(src);
        }
        for req in self.lua.take_picker_opens() {
            // A bad alignment word is a loud echo, then the picker opens centered
            // rather than not at all (the prelude validates, so this is a guard).
            let align = match parse_align(Some(req.align.as_str())) {
                Ok(a) => a,
                Err(e) => {
                    self.editor.echo(format!("btv.picker.open: {e}"));
                    None
                }
            };
            self.editor.open_picker(
                bemtvi_core::MenuPlacement::Editor,
                req.dynamic,
                req.preview,
                parse_extent(&req.width),
                parse_extent(&req.height),
                align,
                build_margin(req.margin),
                if req.prompt_bottom {
                    bemtvi_core::PromptPos::Bottom
                } else {
                    bemtvi_core::PromptPos::Top
                },
                &req.query,
                req.title.clone(),
                req.multiselect,
                req.resumable,
                bemtvi_core::FilterSeed {
                    include: req.include.clone(),
                    exclude: req.exclude.clone(),
                    expanded: req.filters_open,
                    filterable: req.filterable,
                    include_history: req.include_history.clone(),
                    exclude_history: req.exclude_history.clone(),
                },
            );
            self.pending_ui_select = None;
            self.picker_active = true;
            // Kick the source's initial run (generation 0) through the same
            // `picker_query_changes` channel a dynamic query edit uses, rather than
            // running it inline here: the settle fixpoint drains that channel and
            // re-runs `apply_lua_effects` after, so the `btv.run_stream` the source
            // queues (already past this pass's `take_loop_ops`) actually starts. The
            // initial query is the seed (`btv.picker.open{ query = … }`), so a seeded
            // picker opens already filtered; empty is the historical empty-prompt run.
            self.editor
                .picker_query_changes
                .push(bemtvi_core::PickerRun {
                    gen: 0,
                    query: req.query.clone(),
                    include: req.include.clone(),
                    exclude: req.exclude.clone(),
                });
        }
        // `btv.picker.resume()`: replay the last resumable picker's frozen snapshot.
        // Unlike a fresh open, NO gen-0 run is kicked — the snapshot *is* the displayed
        // content (a live-grep order isn't reproducible). The source is re-armed
        // Lua-side, so a later query edit re-runs it (or re-ranks, for a static source).
        if self.lua.take_picker_resume() && self.editor.restore_picker_snapshot() {
            self.pending_ui_select = None;
            self.picker_active = true;
        }
        // `btv.statusline.setup{}` / `reset()`: set the global or a window-local
        // status line (the latest for each target wins). A global / window-local
        // segment layout takes precedence over `'statusline'`; a window `Format`
        // override opts that window back to the `%`-format even under a global
        // layout — see `EditHost::resolve_window_layout`. After any change, recompute
        // the custom-segment set, clear the stale cache, and force a full per-window
        // re-render on the next settle.
        let mut statusline_changed = false;
        for req in self.lua.take_statusline_setups() {
            let segments = |left, right, separator| bemtvi_core::statusline::SegmentLayout {
                left,
                right,
                separator,
            };
            match (req.target, req.kind) {
                (
                    StatuslineTarget::Global,
                    StatuslineKind::Segments {
                        left,
                        right,
                        separator,
                    },
                ) => {
                    self.statusline_layout = Some(segments(left, right, separator));
                }
                // A global `Format` / `Inherit` clears the global layout (back to
                // the `%`-format for every inheriting window).
                (StatuslineTarget::Global, _) => self.statusline_layout = None,
                (
                    StatuslineTarget::Window(w),
                    StatuslineKind::Segments {
                        left,
                        right,
                        separator,
                    },
                ) => {
                    self.statusline_window.insert(
                        WindowId(w),
                        WindowStatusline::Segments(segments(left, right, separator)),
                    );
                }
                (StatuslineTarget::Window(w), StatuslineKind::Format) => {
                    self.statusline_window
                        .insert(WindowId(w), WindowStatusline::Format);
                }
                (StatuslineTarget::Window(w), StatuslineKind::Inherit) => {
                    self.statusline_window.remove(&WindowId(w));
                }
            }
            statusline_changed = true;
        }
        if statusline_changed {
            self.recompute_statusline_custom();
            self.statusline_cache.clear();
            // Force a full per-window render on the next settle.
            self.statusline_layout_key = None;
        }
        // Custom-segment invalidations (`btv.statusline.invalidate`, and the autocmd
        // callbacks a declared `events` list installs): fold into the pending set,
        // re-rendered per window once the input settles.
        self.fold_statusline_invalidates();
        // Custom-segment cell publishes (`btv._statusline_publish`): fold each into
        // the per-`(win, name)` cache the redraw path reads. Produced only while
        // `refresh_statusline_segments` re-renders, so this is empty on the common
        // path.
        self.fold_statusline_publishes();
        // `btv.complete.setup{}`: apply the native completion-engine config. Key
        // notation is parsed here (core stays parser-aware only via `parse_keys`);
        // an empty list keeps that action's built-in default.
        for req in self.lua.take_complete_setups() {
            let mut keys = bemtvi_core::CompleteKeys::default();
            let parse = |list: &[String]| -> Vec<bemtvi_core::input::Key> {
                list.iter()
                    .flat_map(|s| bemtvi_core::input::parse_keys(s))
                    .collect()
            };
            if !req.next.is_empty() {
                keys.next = parse(&req.next);
            }
            if !req.prev.is_empty() {
                keys.prev = parse(&req.prev);
            }
            if !req.confirm.is_empty() {
                keys.confirm = parse(&req.confirm);
            }
            if !req.abort.is_empty() {
                keys.abort = parse(&req.abort);
            }
            self.editor.configure_complete(bemtvi_core::CompleteConfig {
                enabled: true,
                auto: req.auto,
                min_chars: req.min_chars,
                buffer_min_chars: req.buffer_min_chars,
                keys,
                accept: parse_accept_behavior(&req.accept),
                has_async: req.has_async,
                buffer_priority: req.buffer_priority,
                confirm_first: req.confirm_first,
                docs: req.docs,
                docs_wrap: req.docs_wrap,
                trigger_chars: req.trigger_chars.chars().collect(),
            });
            // The built-in `lsp` source is server-native (LSP plumbing + edit
            // application live here, not in Lua/core); remember it + its merge
            // priority so the trigger drain issues `textDocument/completion`. This
            // is feature-agnostic: the wasm edit-host now drives a real language
            // server too (the serverless web demo routes the spawn to an in-browser
            // basedpyright worker), so `lsp` completion works there just like the
            // native build — the same `request_lsp`/`on_completion_reply` round-trip.
            self.complete_lsp_active = req.lsp;
            self.complete_lsp_priority = req.lsp_priority;
            self.complete_lsp_min_chars = req.lsp_min_chars;
            // The built-in `snippets` source is feature-agnostic (the engine is in
            // core), so it works on the wasm build too.
            self.complete_snippets_active = req.snippets;
            self.complete_snippets_priority = req.snippets_priority;
            self.complete_snippets_min_chars = req.snippets_min_chars;
        }
        // `btv.cmdline_complete.setup{}`: enable the command-line completion engine
        // (the float-list widget's fifth orchestration). The last config wins; `docs`
        // toggles the params/help preview pane (Phase 3).
        for docs in self.lua.take_cmdline_complete_setups() {
            self.editor.configure_cmdline_complete(docs);
        }
        // `btv.snippet.setup{}` jump keys, `btv.snippet.add` registrations, and
        // `btv.snippet.expand(body)` immediate expansions.
        for req in self.lua.take_snippet_setups() {
            let parse = |list: &[String]| -> Vec<bemtvi_core::input::Key> {
                list.iter()
                    .flat_map(|s| bemtvi_core::input::parse_keys(s))
                    .collect()
            };
            self.editor
                .set_snippet_keys(parse(&req.next), parse(&req.prev));
        }
        for req in self.lua.take_snippet_adds() {
            self.snippet_add(req.filetype, req.triggers, req.bodies);
        }
        for body in self.lua.take_snippet_expands() {
            match bemtvi_core::parse_snippet(&body) {
                Ok(parsed) => {
                    let row = self.editor.cursor.line;
                    let at = self.editor.buffer().line_start(row) + self.editor.cursor.col;
                    self.editor.expand_snippet(at, at, parsed);
                }
                Err(e) => self.editor.echo(format!("E5900: btv.snippet.expand: {e}")),
            }
        }
        // `btv.complete.trigger()` / a mapped key: manually open the completion
        // popup. Coalesced — one open per drain regardless of how many requests
        // arrived; it ignores `auto` / `min_chars` (an explicit request).
        if !self.lua.take_complete_triggers().is_empty() {
            self.editor.complete_manual_trigger();
        }
        // `btv.complete.accept{ behavior = … }` / a mapped key: accept the highlighted
        // row under an explicit behavior (the remappable alternate to the default
        // confirm key). An empty string uses the engine's configured default. The
        // native edit applies here; a delegated (`lsp` / `snippets`) row sets
        // `complete_accept_request`, drained by the trailing `run_pending`.
        for behavior in self.lua.take_complete_accepts() {
            match behavior.as_str() {
                "" => {
                    self.editor.complete_accept();
                }
                other => {
                    self.editor
                        .complete_accept_with(parse_accept_behavior(other));
                }
            }
        }
        // Picker candidates streamed in: feed them into the open widget,
        // generation-gated — a batch from a query the user has already typed past
        // (`gen` behind the live generation) is dropped, never shown. Coalesced
        // into one `menu_push` so the local matcher re-ranks once per drain.
        let pushes = self.lua.take_picker_pushes();
        if !pushes.is_empty() {
            let live = self.editor.menu_generation();
            let items: Vec<bemtvi_core::MenuItem> = pushes
                .into_iter()
                .filter(|p| p.gen == live)
                .map(|p| bemtvi_core::MenuItem {
                    preview: p.preview.map(|pv| bemtvi_core::PreviewTarget {
                        path: pv.path,
                        loc: pv.loc,
                    }),
                    // The source's two-column row shape (live_grep's location head +
                    // matched line), which the client fits as two columns.
                    layout: p.layout.map(|(head, match_start, match_end, tag)| {
                        bemtvi_core::RowLayout {
                            head,
                            match_start,
                            match_end,
                            tag,
                        }
                    }),
                    // The group the source painted the row with (severity on a
                    // diagnostics row); resolved against the live colorscheme at
                    // projection time, not here.
                    hl: p.hl,
                    ..bemtvi_core::MenuItem::new(p.label, p.key)
                })
                .collect();
            if !items.is_empty() {
                // `live` is the generation: a newer one atomically replaces the
                // still-displayed older results (no flash-empty while typing).
                self.editor.menu_push(items, live);
            }
        }
        // A source run that *completed* (`done()`): for the live query, if nothing
        // streamed in, the query matched nothing — clear the now-stale results.
        // Gated on the live generation so a killed older run's `done()` is ignored.
        for gen in self.lua.take_picker_finishes() {
            if gen == self.editor.menu_generation() {
                self.editor.menu_finish(gen);
            }
        }
        // Async completion candidates streamed in: append them to the open
        // completion popup, generation-gated exactly like the picker — a batch from a
        // prefix the user has typed past (`gen` behind the live completion
        // generation) is dropped. Each carries its accept `insert` text. Coalesced
        // into one `menu_push` so the prefix matcher re-ranks the batch once.
        let cpushes = self.lua.take_complete_pushes();
        if !cpushes.is_empty() {
            let live = self.editor.menu_generation();
            let items: Vec<bemtvi_core::MenuItem> = cpushes
                .into_iter()
                .filter(|p| p.gen == live)
                .map(|p| bemtvi_core::MenuItem {
                    // A plugin source's declared kind (`push { kind = … }`), shown
                    // right-aligned on the row; `None` when the item omits it.
                    kind: p.kind,
                    insert: Some(p.insert),
                    // The source's merge priority, so an async source ranks against
                    // buffer/lsp by priority instead of pinning at the `0` floor.
                    priority: p.priority,
                    source_accept: p.accept.is_some(),
                    // A plugin source can attach inline docs (`push { doc = … }`),
                    // rendered beside the popup for the selected row (Phase 4-E).
                    doc: p.doc,
                    // Or a lazy-docs `resolve` handle, resolved on selection.
                    resolve: p.resolve,
                    // A plain row inserts natively by `insert` (key unused). A row whose
                    // item carried an `on_accept` delegates its accept to Lua: it rides a
                    // key in the plugin-accept range and `source_accept = true`, so core
                    // records the accept for the drain rather than splicing `insert`.
                    ..bemtvi_core::MenuItem::new(
                        p.label,
                        p.accept
                            .map_or(0, |id| crate::snippet::PLUGIN_ACCEPT_KEY_BASE + id as usize),
                    )
                })
                .collect();
            if !items.is_empty() {
                self.editor.menu_push(items, live);
            }
        }
        // An async source set (all sources for a generation) finished: close the
        // popup if, across the buffer seed and every async source, the live prefix
        // matched nothing (a confirmed-empty completion has no prompt to keep up).
        for gen in self.lua.take_complete_finishes() {
            self.editor.complete_finish(gen);
        }
        // A plugin row's `resolve` callback responded with lazy docs: cache them by
        // handle (even `""` ⇒ resolved-but-docless, so the row is never re-resolved)
        // and repaint the sidebar (Phase 4-E). Native-only — the docs sidebar is.
        #[cfg(feature = "native")]
        for (id, doc) in self.lua.take_complete_resolve_dones() {
            if self.complete_resolve_inflight == Some(id) {
                self.complete_resolve_inflight = None;
            }
            self.complete_resolve_docs.insert(id, doc);
            self.lsp_dirty = true;
        }
        #[cfg(not(feature = "native"))]
        let _ = self.lua.take_complete_resolve_dones();
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
            // Parsed the way this client's keys are parsed (`input`) and the way a
            // mapping's LHS is compiled (`Keymaps::build_for`): under the kitty keyboard
            // protocol `<C-h>`/`<C-i>`/`<C-m>`/`<C-[>` stay distinct, otherwise they fold
            // onto `<BS>`/`<Tab>`/`<CR>`/`<Esc>`. Folding unconditionally here would put
            // typeahead out of step with the maps it is fed through — a fed `<C-h>` would
            // arrive as `<BS>` and miss the `<C-h>` mapping that a protocol-on client
            // registered.
            let keys = if self.keyboard_protocol {
                bemtvi_core::parse_keys_raw(&op.keys)
            } else {
                bemtvi_core::parse_keys(&op.keys)
            };
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

    /// Push any escape sequences the tick queued for the client's *terminal*
    /// (today: the OSC 52 clipboard write behind a `"+` / `"*` yank) as
    /// `btv_ui_send` notifications — the client writes each verbatim to its tty.
    /// Called from [`redraw`](crate::EditHost::redraw), so a write always travels
    /// with the frame of the tick that produced it. A no-op unless this session's
    /// clipboard is the terminal (see
    /// [`ClipboardProvider::Osc52`](crate::ClipboardProvider::Osc52)).
    #[cfg(feature = "native")]
    pub(crate) fn flush_ui_sends(&mut self) {
        let Some(state) = self.osc52.as_ref() else {
            return;
        };
        let pending = std::mem::take(
            &mut state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending,
        );
        for seq in pending {
            self.fx.notify("btv_ui_send", vec![Value::from(seq)]);
        }
    }

    /// Drain the status-line clicks the core recorded for the last mouse gesture
    /// (`%@handler@…%X` regions) and fire each one's Lua handler. For every click,
    /// recompute the window's click regions and resolve the clicked column to a
    /// handler (a `v:lua.…` reference) + `minwid`; a click that lands outside every
    /// region is a no-op (the window was already focused by the core). The handler is
    /// called with neovim's arguments `(minwid, clicks, button, modifiers)`; its
    /// queued effects drain through `apply_lua_effects` + `run_pending`, so a handler
    /// that runs `vim.cmd(...)` / opens a picker settles like any other Lua entry.
    pub(crate) fn dispatch_statusline_clicks(&mut self) {
        use bemtvi_core::statusline::ClickAction;
        let clicks = std::mem::take(&mut self.editor.statusline_clicks);
        if clicks.is_empty() {
            return;
        }
        let mut fired = false;
        for click in clicks {
            let Some(action) = self.statusline_click_at(click.win.0, click.col, click.surface)
            else {
                continue;
            };
            match action {
                // `%@handler@` / a segment `on_click`: fire the Lua handler with
                // neovim's click arguments.
                ClickAction::Handler { handler, minwid } => {
                    if let Err(e) = self.lua.run_statusline_click(
                        &handler,
                        minwid,
                        click.clicks,
                        click.button,
                        &click.modifiers,
                    ) {
                        self.editor.echo(format!("E:statusline click handler: {e}"));
                    }
                }
                // `%nT`: switch the main region to tab page `n` (core action).
                ClickAction::Tab(n) => self.editor.select_main_tab(n),
            }
            fired = true;
        }
        // Apply the handlers' queued effects and drive them to convergence — the
        // mouse arm in `dispatch` doesn't `run_pending` on its own (a bare click
        // changes only core state), so a handler's `vim.cmd`/`:lua` would otherwise
        // wait for the next keystroke to settle.
        if fired {
            self.apply_lua_effects();
            self.run_pending();
        }
    }

    /// Apply one [`BufOp`] to the live editor: a buffer-local option write (`vim.bo` /
    /// `nvim_set_option_value` with a `buf`). The buffer-*text* / lifecycle mutation
    /// surface (`nvim_buf_set_lines`, `nvim_create_buf`, `nvim_buf_delete`) is
    /// intentionally absent from bemtvi's Lua API (see `bemtvi-lua`'s `prelude/api.lua`
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
            BufOp::SetGlobalOption { name, value } => {
                // The *global value* of a buffer-local option (the `:setglobal` tier).
                // Routed through the same core entry points the `:setglobal` ex path
                // uses, so the validation and the loud "no global value" rejection are
                // shared rather than re-implemented on the Lua leg.
                match value {
                    OptionValue::Number(n) => self.editor.set_buf_global_option_num(&name, n),
                    OptionValue::Bool(b) => self.editor.set_buf_global_option_bool(&name, b),
                    OptionValue::String(s) => self.editor.set_buf_global_option_str(&name, &s),
                }
            }
            BufOp::SetLines {
                bufnr,
                start,
                end,
                lines,
            } => {
                // The lone buffer-text mutation. `api_set_lines` fails loud on a
                // read-only / gone buffer — surface that as a message rather than a
                // silent no-op (the Lua front already rejected the common bad shapes).
                if let Err(e) = self
                    .editor
                    .api_set_lines(BufferId(bufnr), start, end, &lines)
                {
                    self.editor.echo(e);
                }
            }
            BufOp::SetText {
                bufnr,
                start_row,
                start_col,
                end_row,
                end_col,
                lines,
            } => {
                // The precise-range text mutation. Like `api_set_lines` it fails loud on
                // a read-only / gone buffer or an inverted span — surface it as a message.
                if let Err(e) = self.editor.api_set_text(
                    BufferId(bufnr),
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    &lines,
                ) {
                    self.editor.echo(e);
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
                decor,
                right_gravity,
                end_right_gravity,
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
                let decor = decor.map(|d| Box::new(virt_decor_to_core(*d)));
                if let Some(buf) = self.editor.buffer_of_mut(bid) {
                    buf.extmarks.set_with_gravity(
                        ns,
                        Some(id),
                        start,
                        end,
                        hl_group,
                        priority,
                        decor,
                        right_gravity,
                        end_right_gravity,
                    );
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
            WindowOp::SelectRange {
                win,
                s_row,
                s_col,
                e_row,
                e_col,
                escape_insert,
            } => {
                let id = resolve_win(self, win);
                self.editor
                    .select_range_in_window(id, s_row, s_col, e_row, e_col, escape_insert);
            }
            WindowOp::Jump {
                path,
                line,
                col,
                target,
                to_main,
            } => {
                // The picker has closed and returned focus by confirm time. The confirm
                // gesture's target opens a fresh tab / split, or — for `Current` —
                // navigates the focused window honoring 'switchbuf', reusing an open
                // buffer without a reload/modified guard. See the op's doc comment.
                // `to_main` (the source's `layer = "main"`) crosses out of a dock first,
                // so a picked file lands in the editor and not the sidebar it was
                // launched from.
                use bemtvi_lua::OpenTarget;
                if to_main {
                    self.editor.ensure_main_layer();
                }
                let p = std::path::Path::new(&path);
                match target {
                    OpenTarget::Current => self.editor.jump_to(p, line, col),
                    OpenTarget::Tab => self.editor.jump_to_tab(p, line, col),
                    OpenTarget::Split => self.editor.jump_to_split(p, line, col, false),
                    OpenTarget::Vsplit => self.editor.jump_to_split(p, line, col, true),
                }
            }
            WindowOp::OpenSwitchbuf { path, to_main } => {
                // Open honoring 'switchbuf' (the picker's location-less file confirm);
                // no forced cursor — a reused window keeps its place. `to_main` crosses
                // out of a dock first (see `WindowOp::Jump`).
                if to_main {
                    self.editor.ensure_main_layer();
                }
                // The picker's location-less file confirm is a jump (like `:edit`):
                // record the position we leave so `<C-o>` returns here.
                self.editor
                    .open_path_switchbuf_as_jump(std::path::Path::new(&path));
            }
            WindowOp::BufSwitch {
                buf,
                target,
                to_main,
            } => {
                use bemtvi_lua::OpenTarget;
                if to_main {
                    self.editor.ensure_main_layer();
                }
                let id = BufferId(buf);
                match target {
                    OpenTarget::Current => self.editor.switch_to_buffer_switchbuf(id),
                    OpenTarget::Tab => self.editor.open_buffer_in_tab(id),
                    OpenTarget::Split => self.editor.open_buffer_in_split(id, false),
                    OpenTarget::Vsplit => self.editor.open_buffer_in_split(id, true),
                }
            }
            WindowOp::SetTopline { win, top } => {
                let id = resolve_win(self, win);
                self.editor.set_window_topline(id, top);
            }
            WindowOp::SetLeftcol { win, leftcol } => {
                let id = resolve_win(self, win);
                self.editor.set_window_leftcol(id, leftcol);
            }
            WindowOp::SetWidth { win, width } => {
                let id = resolve_win(self, win);
                self.editor.set_window_width(id, width);
            }
            WindowOp::SetHeight { win, height } => {
                let id = resolve_win(self, win);
                self.editor.set_window_height(id, height);
            }
            WindowOp::SetGlobalOption { name, value } => {
                // The *global value* of a window-local option (the `:setglobal` tier),
                // routed through the same core entry points the ex path uses.
                match value {
                    OptionValue::Bool(b) => self.editor.set_win_global_option_bool(&name, b),
                    OptionValue::Number(n) => self.editor.set_win_global_option_num(&name, n),
                    OptionValue::String(s) => self.editor.set_win_global_option_str(&name, &s),
                }
            }
            WindowOp::SetOption { win, name, value } => {
                let id = resolve_win(self, win);
                // Window-local options span all three kinds: booleans
                // (number / relativenumber), numbers (numberwidth), and strings
                // (signcolumn). Route each to the matching typed setter; a kind that
                // doesn't fit the named option is ignored rather than coerced.
                match value {
                    OptionValue::Bool(b) => self.editor.set_window_option_bool(id, &name, b),
                    OptionValue::Number(n) => self.editor.set_window_option_num(id, &name, n),
                    OptionValue::String(s) => self.editor.set_window_option_str(id, &name, &s),
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
                align,
                margin,
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
                let cur_win = self.editor.current_window_id();
                let config = match build_float_config(
                    &relative,
                    win,
                    cur_win,
                    &anchor,
                    row,
                    col,
                    &width,
                    &height,
                    align.as_deref(),
                    margin,
                    zindex,
                    focusable,
                    &border,
                    title,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        self.editor.echo(format!("nvim_open_win: {e}"));
                        return;
                    }
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
                align,
                margin,
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
                // A size key present but unparseable is a loud error rather than a
                // silent no-op (an absent key stays `None` ⇒ unchanged).
                if let Some(spec_str) = width.as_deref() {
                    match parse_extent(spec_str) {
                        Some(e) => spec.width = Some(e),
                        None => {
                            self.editor.echo(format!(
                                "nvim_win_set_config: invalid 'width': '{spec_str}'"
                            ));
                            return;
                        }
                    }
                }
                if let Some(spec_str) = height.as_deref() {
                    match parse_extent(spec_str) {
                        Some(e) => spec.height = Some(e),
                        None => {
                            self.editor.echo(format!(
                                "nvim_win_set_config: invalid 'height': '{spec_str}'"
                            ));
                            return;
                        }
                    }
                }
                // `align`: absent ⇒ unchanged; `""` ⇒ clear to anchor/offset form;
                // a word ⇒ set it (a bad word is a loud error).
                if let Some(word) = align.as_deref() {
                    if word.is_empty() {
                        spec.align = Some(None);
                    } else {
                        match bemtvi_core::Align::from_keyword(word) {
                            Some(a) => spec.align = Some(Some(a)),
                            None => {
                                self.editor.echo(format!(
                                    "nvim_win_set_config: invalid 'align': '{word}'"
                                ));
                                return;
                            }
                        }
                    }
                }
                spec.margin = margin.map(build_margin);
                spec.row = row.map(|v| v as isize);
                spec.col = col.map(|v| v as isize);
                spec.zindex = zindex;
                spec.focusable = focusable;
                // A present title key sets (or, empty, clears) it — the same
                // `normalize_title` policy as the msgpack decoder.
                spec.title = title.map(|t| normalize_title(Some(t)));
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

    /// Refresh the Lua **current-buffer snapshot** (`btv._cur_buf`: number, name,
    /// filetype) to the editor's current buffer — the current-buffer-identity twin of
    /// [`push_buf_mirror`](Self::push_buf_mirror)'s content refresh. A Lua getter for the
    /// *current* buffer (`vim.fn.expand("%")` / `%:p`, the filetype) reads this, so it
    /// must track the current buffer after every batch — otherwise it lags at whatever
    /// the last autocmd left (e.g. empty right after `:edit`, before any buffer event).
    pub(crate) fn refresh_cur_buf_snapshot(&mut self) {
        let buf = self.editor.current_buffer_id();
        let name = self.editor.display_name(buf);
        let ft = crate::filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, &name, ft);
    }

    /// Fold buffer `id`'s drained mirror edit batch into one [`LinesDelta`] — the
    /// rows to splice into the mirror array Lua already holds, instead of
    /// re-serializing the whole buffer. `prev_count` is the line count the mirror
    /// was last pushed at, which bounds the replaced span.
    ///
    /// `None` means "push the buffer in full": either the batch can't be folded
    /// soundly (see [`fold_mirror_edits`]) or the folded span doesn't fit the mirror
    /// Lua holds — a safety valve, since applying a span that overruns the array
    /// would silently corrupt it.
    fn mirror_delta(
        &self,
        id: BufferId,
        prev_count: usize,
        batch: &bemtvi_core::EditBatch,
    ) -> Option<LinesDelta> {
        // A whole-rope replacement (undo/redo, `:e`, reload) invalidates every row
        // anchor, so there is nothing to splice onto.
        if batch.resync {
            return None;
        }
        let (start, old_end_row, new_end_row) = fold_mirror_edits(&batch.edits)?;
        // The journal's points are *positions*; the rows they touch are inclusive at
        // both ends, so the end-exclusive line span runs one past `old_end_row` /
        // `new_end_row` (the same conversion `on_lines` makes for its
        // `lastline`/`new_lastline`).
        let new_end = new_end_row + 1;
        // An edit ending at the very end of the buffer has its `old_end_point` on the
        // rope's phantom trailing line, which is never mirrored — so the span can run
        // exactly one row past the mirrored count, and clamping it there is the
        // conversion, not a fudge. Anything further past the end is an inconsistency
        // between the fold and the mirror, so bail to a full push rather than splice a
        // span that would overrun the array.
        let old_end = (old_end_row + 1).min(prev_count);
        if old_end_row > prev_count || start > prev_count {
            return None;
        }
        let lines = self.editor.lines_range_of(id, start, new_end)?;
        Some(LinesDelta {
            start: start as u64,
            old_end: old_end as u64,
            lines,
        })
    }

    /// Refresh the Rust→Lua buffer mirror (`btv._bufs` + `btv._cur_cursor` +
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
        // Which buffers live in the focused layer, for `btv.buf.list{ focused = true }`
        // (the per-region buffer list — see `OpenBuffer::layer` in core).
        let focused_bufs: HashSet<BufferId> =
            self.editor.focused_buffer_ids().into_iter().collect();
        // Buffer-local option values, mirrored so `vim.bo` / `nvim_get_option_value`
        // read the core's current value (the default until set, and values set via
        // the `:set` ex path). A row is re-pushed only when something that can move
        // it changed since the last push: the core's option-state generation (any
        // `:set`-family / `vim.o` / `btv.wso` write, a filetype or `ts_highlight`
        // change, a completed save), or the buffer's own `changedtick` (a text edit
        // flips `modified`). An untouched push — cursor moved, nothing else — skips
        // the rebuild instead of re-serializing every buffer's row.
        let opts_moved = self.bo_mirror_gen != self.editor.options_generation();
        if opts_moved {
            self.bo_mirror_gen = self.editor.options_generation();
        }
        let mut bo: Vec<BoMirror> = Vec::new();
        // The extmark snapshot for `nvim_buf_get_extmarks`: only buffers that hold
        // marks contribute, so a session with no decoration plugin pays nothing.
        // `None` for a buffer whose mark set did not change structurally — Lua keeps
        // the decorations it already holds and only its positions are refreshed
        // (below), which is the whole point: an edit moves anchors, never payloads.
        let mut extmarks: Vec<(u64, Option<Vec<ExtmarkMirror>>)> = Vec::new();
        let mut extmark_positions_by_buf: Vec<(u64, Vec<i64>)> = Vec::new();
        // Buffers whose text changed since the last mirror, for the `nvim_buf_attach`
        // `on_lines` callbacks: `(bufnr, changedtick, old_line_count, new_line_count)`.
        // Only buffers already known last push contribute (a buffer's first
        // appearance is its creation, not an edit), and the callbacks are fired
        // *after* every mirror is consistent (below), so a callback reading
        // `nvim_buf_get_lines` sees the new content.
        let mut changed: Vec<(u64, u64, usize, usize)> = Vec::new();
        // Buffers whose text changed since the last mirror, paired with whether they
        // were `known` last push, so the byte-delta drain below (the `nvim_buf_attach`
        // `on_bytes` channel) can fire for the ones a plugin could be attached to and
        // discard a first-seen buffer's pre-attach deltas. Carries the changedtick to
        // stamp the `on_bytes` callback with.
        let mut fresh_ids: Vec<(BufferId, bool, u64)> = Vec::new();
        for id in self.editor.buffer_ids() {
            let tick = self
                .editor
                .buffer_of(id)
                .map(|b| b.changedtick)
                .unwrap_or(0);
            let known = self.buf_mirror_ticks.contains_key(&id);
            let fresh = self.buf_mirror_ticks.get(&id) != Some(&tick);
            // A changed buffer ships either the rows that changed (a `LinesDelta`,
            // spliced into the array Lua already holds) or the whole array. The delta
            // is what keeps an edit O(changed rows) instead of O(buffer); the full
            // push is the fallback for a buffer Lua has never seen, a whole-rope
            // replacement, and an unfoldable batch. See
            // `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
            let (mut lines, mut delta) = (None, None);
            // Which of this buffer's marks the batch can have moved in row/col terms.
            // `All` until a batch bounds it (and for a buffer that didn't change, it
            // is never consulted — no positions are pushed at all).
            let mut pos_scope = PosScope::All;
            if fresh {
                self.buf_mirror_ticks.insert(id, tick);
                fresh_ids.push((id, known, tick));
                // Drained unconditionally on a changed buffer, so the journal can't
                // accumulate behind a full push.
                let batch = self.editor.take_mirror_edits_of(id).unwrap_or_default();
                if let Some(b) = self.editor.buffer_of(id) {
                    if let Some(scope) = extmark_dirty_window(b, &batch) {
                        pos_scope = scope;
                    }
                }
                let prev_count = self.buf_mirror_lines.get(&id).copied();
                delta = match (known, prev_count) {
                    (true, Some(prev)) => self.mirror_delta(id, prev, &batch),
                    _ => None,
                };
                if delta.is_none() {
                    lines = Some(self.editor.lines_of(id).unwrap_or_default());
                }
                // The line counts the `on_lines` callbacks report. Taken from the
                // core rather than the pushed array, which a delta no longer carries
                // whole.
                let new_count = self.editor.line_count_of(id).unwrap_or(0);
                if known {
                    changed.push((id.0, tick, prev_count.unwrap_or(new_count), new_count));
                }
                self.buf_mirror_lines.insert(id, new_count);
            }
            let name = self.editor.display_name(id);
            if let Some(b) = self.editor.buffer_of(id) {
                let o = b.options;
                if opts_moved || fresh {
                    bo.push(BoMirror {
                        bufnr: id.0,
                        tabstop: o.tabstop,
                        shiftwidth: o.shiftwidth,
                        softtabstop: o.softtabstop,
                        expandtab: o.expandtab,
                        autoindent: o.autoindent,
                        smartindent: o.smartindent,
                        autopairs: o.autopairs,
                        indentemptylines: o.indentemptylines,
                        regexsyntax: self.editor.resolve_regexsyntax(o.regexsyntax).to_string(),
                        fileencoding: o.fileencoding.to_string(),
                        bomb: o.bomb,
                        fileformat: o.fileformat.to_string(),
                        endofline: o.endofline,
                        fixendofline: o.fixendofline,
                        modified: b.modified,
                        filetype: self.editor.buffer_filetype(id).unwrap_or_default(),
                        ts_highlight: self.editor.ts_highlight_enabled(id),
                        commentstring: self.editor.effective_commentstring(id),
                        modifiable: o.modifiable,
                        buftype: self.editor.buffer_buftype(id).to_string(),
                        foldmethod: o.foldmethod.to_string(),
                        // The *effective* values, like `commentstring` above: a buffer with
                        // no entry of its own follows the global tier, and `vim.bo` must
                        // report what folding actually uses.
                        foldexpr: self.editor.effective_foldexpr(id).to_string(),
                        foldmarker: {
                            let (open, close) = self.editor.effective_foldmarker_of(id);
                            format!("{open},{close}")
                        },
                        foldnestmax: o.foldnestmax,
                        foldminlines: o.foldminlines,
                        undolevels: o.undolevels,
                    });
                    self.bo_mirror_known.insert(id);
                }
                // A buffer contributes a full re-serialize only when its store's
                // structural generation moved (a mark set / deleted / cleared). An
                // untouched buffer contributes nothing at all, and an edited one
                // contributes positions only.
                let ext_gen = b.extmarks.generation();
                let structural = self.extmark_gens.get(&id) != Some(&ext_gen);
                if structural {
                    self.extmark_gens.insert(id, ext_gen);
                }
                if b.extmarks.is_empty() {
                    // No marks: the buffer contributes nothing and Lua drops it.
                } else if !structural {
                    extmarks.push((id.0, None));
                    if fresh {
                        let flat = extmark_positions(b, pos_scope);
                        if !flat.is_empty() {
                            extmark_positions_by_buf.push((id.0, flat));
                        }
                    }
                } else {
                    let marks = b
                        .extmarks
                        .iter_with_ns()
                        // The reserved multi-cursor namespaces (cursor heads and
                        // their visual anchors) are internal editor state, not
                        // user-visible extmarks — keep them out of the
                        // `nvim_buf_get_extmarks` mirror.
                        .filter(|(ns, _)| !is_reserved_ns(*ns))
                        .map(|(ns, m)| {
                            let (row, col) = byte_rowcol(b, m.start);
                            let (end_row, end_col) = match m.end {
                                Some(e) => {
                                    let (r, c) = byte_rowcol(b, e);
                                    (Some(r), Some(c))
                                }
                                None => (None, None),
                            };
                            let d = m.decor.as_deref();
                            let (sign_text, sign_hl_group) = match d {
                                Some(d) => (d.sign_text.clone(), d.sign_hl_group.clone()),
                                None => (None, None),
                            };
                            let (line_fill_text, line_fill_hl) =
                                match d.and_then(|d| d.line_fill.as_ref()) {
                                    Some(f) => (Some(f.text.clone()), f.hl_group.clone()),
                                    None => (None, None),
                                };
                            let line_hl_group = d.and_then(|d| d.line_hl_group.clone());
                            ExtmarkMirror {
                                ns,
                                id: m.id,
                                row,
                                col,
                                end_row,
                                end_col,
                                hl_group: m.hl_group.clone(),
                                priority: m.priority,
                                sign_text,
                                sign_hl_group,
                                line_fill_text,
                                line_fill_hl,
                                line_hl_group,
                                right_gravity: m.right_gravity,
                                end_right_gravity: m.end_right_gravity,
                            }
                        })
                        .collect();
                    extmarks.push((id.0, Some(marks)));
                }
            }
            bufs.push(BufMirror {
                bufnr: id.0,
                lines,
                delta,
                name,
                changedtick: tick,
                focused: focused_bufs.contains(&id),
                // 1-based, the `:ls` convention (and neovim's `getbufinfo().lnum`).
                lnum: self.editor.buffer_last_line(id).unwrap_or(0) as u64 + 1,
            });
        }
        // Drop tick entries for buffers that no longer exist, so the map can't grow
        // unboundedly across a long session of opening and closing buffers.
        let live: HashSet<BufferId> = self.editor.buffer_ids().into_iter().collect();
        self.buf_mirror_ticks.retain(|id, _| live.contains(id));
        self.buf_mirror_lines.retain(|id, _| live.contains(id));
        self.extmark_gens.retain(|id, _| live.contains(id));
        // The `bo` mirror merges rows into the Lua table, so a buffer that vanished
        // since the last push is dropped from the table explicitly — `vim.bo` for a
        // dead bufnr reads `nil` exactly as it did when the table was replaced whole.
        let removed: Vec<u64> = self
            .bo_mirror_known
            .iter()
            .filter(|id| !live.contains(*id))
            .map(|id| id.0)
            .collect();
        self.bo_mirror_known.retain(|id| live.contains(id));

        // Drain each changed buffer's byte-delta journal and project it into neovim's
        // `on_bytes` tuple for the `nvim_buf_attach` `on_bytes` callbacks (fired below,
        // once the mirrors are consistent). A `resync` batch (undo/redo/`:e`) can't be
        // replayed as deltas — signal a reload (the `on_reload` callback) so an
        // attached consumer re-reads the buffer whole instead. A first-seen (`!known`)
        // buffer's pre-attach deltas are discarded: no callback is attached yet.
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
        // dimensions, so the `nvim_win_*` getters read live state from Lua. It
        // spans *every* tab (like `nvim_list_wins` / `win_findbuf` in neovim), so
        // a window in another tab is still a valid, findable handle from Lua; the
        // current tab's layout order is pushed alongside it (`cur_wins`) for the
        // window-*number* surface (`winnr()` / `win_getid()`), which is per-tab.
        let global_scrollanim = self.editor.global_options().scrollanim;
        let win_ids = self.editor.all_window_ids();
        let wins: Vec<WindowMirror> = win_ids
            .iter()
            .copied()
            .map(|id| {
                let buffer = self.editor.window_buffer(id).map(|b| b.0).unwrap_or(0);
                let (line, col) = self.editor.window_cursor(id).unwrap_or((0, 0));
                let (cw, ch) = self.editor.window_content_size(id).unwrap_or((0, 0));
                let opts = self.editor.window_options(id).unwrap_or_default();
                let (top, leftcol) = self.editor.window_scroll(id).unwrap_or((0, 0));
                // The jumplist is gated on its per-window generation (the same
                // structural gate the extmark mirror uses): unchanged since the last
                // push, the row carries an empty list and the Lua side keeps the old
                // one — a repaint never re-serializes a whole jumplist. The pointer
                // is read fresh either way (it is one usize; a moved generation is
                // exactly when it changed).
                let gen = self.editor.window_jumplist_gen(id).unwrap_or(0);
                let moved = self.win_jump_gens.get(&id).copied() != Some(gen);
                if moved {
                    self.win_jump_gens.insert(id, gen);
                }
                let (jumps, jump_idx) = if moved {
                    self.editor.window_jumplist(id).unwrap_or_default()
                } else {
                    (Vec::new(), self.editor.window_jumplist_idx(id).unwrap_or(0))
                };
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
                    cursorline: opts.cursorline,
                    wrap: opts.wrap,
                    scrollanim: opts.scrollanim.unwrap_or(global_scrollanim),
                    numberwidth: opts.numberwidth as u64,
                    scrolloff: opts.scrolloff as u64,
                    colorcolumn: opts.colorcolumn.clone(),
                    signcolumn: opts.signcolumn.to_string(),
                    fillchars: opts.fillchars.clone(),
                    padding: opts.padding.to_string(),
                    breakindent: opts.breakindent,
                    showbreak: opts.showbreak.clone(),
                    breakindentopt: opts.breakindentopt.clone(),
                    sidescroll: opts.sidescroll as u64,
                    sidescrolloff: opts.sidescrolloff as u64,
                    foldcolumn: opts.foldcolumn as u64,
                    foldenable: opts.foldenable,
                    foldlevel: opts.foldlevel as u64,
                    winhighlight: opts.winhighlight.clone(),
                    // `winsaveview()` reports `topline` 1-based; `top` is 0-based.
                    topline: (top + 1) as u64,
                    leftcol: leftcol as u64,
                    float: self
                        .editor
                        .window_float_config(id)
                        .map(|cfg| float_mirror(cfg, cw, ch)),
                    jumps: jumps
                        .into_iter()
                        .map(|(bufnr, line, col)| JumpMirror {
                            bufnr,
                            lnum: (line + 1) as u64, // 1-based row, neovim convention
                            col: col as u64,         // 0-based byte column
                            coladd: 0,               // bemtvi has no `virtualedit`
                        })
                        .collect(),
                    jump_idx: jump_idx as u64,
                    jump_gen: gen,
                }
            })
            .collect();
        // Prune the jumplist generations of windows that closed — a recycled id
        // pushes its full list again.
        self.win_jump_gens.retain(|id, _| win_ids.contains(id));
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
        // The last mouse event's position (`vim.fn.getmousepos()`), resolved through the
        // core hit-test, so a mouse mapping fired below reads the clicked cell.
        let mp = self.editor.mouse_pos();
        let _ = self.lua.set_mouse_pos(
            mp.screenrow,
            mp.screencol,
            mp.winid,
            mp.winrow,
            mp.wincol,
            mp.line,
            mp.column,
        );
        let cur_wins: Vec<u64> = self.editor.window_ids().into_iter().map(|w| w.0).collect();
        let _ = self.lua.set_buf_mirror(
            &bufs,
            cursor,
            cur_win,
            &wins,
            &cur_wins,
            next_win,
            self.editor.mode.short_code(),
            self.editor.cmdline_type(),
        );
        // The alternate file name (`#`), the twin of the `%` name in the current-buffer
        // snapshot — refreshed here so `vim.fn.expand("#")` tracks the same `#` the
        // ex-command token expands to.
        let _ = self
            .lua
            .set_alt_file(&self.editor.alternate_file_name().unwrap_or_default());
        // …and `#` as a live handle beside it (`0` ⇒ no alternate), which is the form a
        // buffer list marks a row with — see `LuaRuntime::set_alt_buf`.
        let _ = self
            .lua
            .set_alt_buf(self.editor.alternate_buffer().map_or(0, |b| b.0));
        // Only the rows that moved since the last push, plus the bufnrs to drop from
        // the Lua table; an untouched push (no option moved, no buffer edited) sends
        // nothing at all.
        if !bo.is_empty() || !removed.is_empty() {
            let _ = self.lua.set_bo_mirror(&bo, &removed);
        }
        // …and the tier those buffers were born from, so `vim.go.tabstop` /
        // `vim.opt_global` read the core's global value rather than a Lua-side echo.
        let g = self.editor.buf_opts_global();
        let _ = self.lua.set_bo_global_mirror(&bemtvi_lua::BoGlobalMirror {
            tabstop: g.tabstop,
            shiftwidth: g.shiftwidth,
            softtabstop: g.softtabstop,
            expandtab: g.expandtab,
            autoindent: g.autoindent,
            smartindent: g.smartindent,
            autopairs: g.autopairs,
            indentemptylines: g.indentemptylines,
            fixendofline: g.fixendofline,
            foldmethod: g.foldmethod.to_string(),
            foldnestmax: g.foldnestmax,
            foldminlines: g.foldminlines,
            undolevels: g.undolevels,
            commentstring: self.editor.commentstring_global().to_string(),
            foldexpr: self.editor.foldexpr_global().to_string(),
            foldmarker: {
                let (open, close) = self.editor.foldmarker_global();
                format!("{open},{close}")
            },
        });
        // …and the window tier beside it, for `vim.go.number` / `vim.opt_global.scrolloff`.
        let w = self.editor.win_opts_global();
        let _ = self.lua.set_wo_global_mirror(&bemtvi_lua::WoGlobalMirror {
            number: w.number,
            relativenumber: w.relativenumber,
            cursorline: w.cursorline,
            wrap: w.wrap,
            breakindent: w.breakindent,
            foldenable: w.foldenable,
            scrolloff: w.scrolloff,
            sidescroll: w.sidescroll,
            sidescrolloff: w.sidescrolloff,
            numberwidth: w.numberwidth,
            foldcolumn: w.foldcolumn,
            foldlevel: w.foldlevel,
            signcolumn: w.signcolumn.to_string(),
            colorcolumn: w.colorcolumn.clone(),
            showbreak: w.showbreak.clone(),
            breakindentopt: w.breakindentopt.clone(),
            fillchars: w.fillchars.clone(),
            padding: w.padding.to_string(),
            winhighlight: w.winhighlight.clone(),
        });
        let _ = self
            .lua
            .set_extmark_mirror(&extmarks, &extmark_positions_by_buf);
        // The highlight registry, mirrored so `nvim_get_hl` reads live group
        // definitions from Lua. Gated on the registry's generation — a colorscheme
        // populates hundreds of groups once and rarely changes them, so re-pushing
        // the whole table every chunk would be wasteful; only a real change (a
        // `:hi` / `nvim_set_hl` / `:colorscheme`) re-serializes it.
        let hl_gen = self.editor.highlights.generation();
        if self.hl_mirror_gen != Some(hl_gen) {
            self.hl_mirror_gen = Some(hl_gen);
            let mirror = |ns: u32, name: &str, def: &bemtvi_core::highlight::HlDef| HlDefMirror {
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
            // Non-zero namespaces ride a separate mirror (`btv._hl_defs_ns`) so
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
        // Live `btv.view` surfaces, mirrored so a view's `:set_decor` (extmarks on the
        // backing buffer) and `:line()` read the current buffer number / cursor line
        // without a server round-trip. Cheap (one entry per open view, usually zero).
        let _ = self.lua.set_view_mirror(&self.editor.view_mirror());
        // The plugin views a session restore reserved a slot for but no plugin has
        // adopted yet (`btv.view.pending_restores()` / the `on_restore` dispatch). Usually
        // empty (only just after a restore that carried persisted views).
        let _ = self
            .lua
            .set_view_pending(&self.editor.view_pending_restores());
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
            pummaxwidth: go.pummaxwidth as u64,
            report: go.report as u64,
            showcmd: go.showcmd,
            statusline: go.statusline.clone(),
            tabline: go.tabline.clone(),
            guifont: go.guifont.clone(),
            guiglyphoverflow: go.guiglyphoverflow.clone(),
            regexsyntax: go.regexsyntax.clone(),
            fileencodings: go.fileencodings.clone(),
            autoread: go.autoread,
            imagepreview: go.imagepreview,
            httphost: go.httphost.clone(),
            httpport: go.httpport,
            scrollanim: go.scrollanim,
            scrollanimduration: go.scrollanimduration as u64,
            scrollback: go.scrollback as u64,
            history: go.history as u64,
            persisthistory: go.persisthistory.clone(),
            timeout: go.timeout,
            timeoutlen: go.timeoutlen as u64,
            columns: columns as u64,
            lines: lines as u64,
            errorformat: go.errorformat.clone(),
            switchbuf: go.switchbuf.clone(),
            makeprg: go.makeprg.clone(),
            grepprg: go.grepprg.clone(),
            grepformat: go.grepformat.clone(),
            qfdock: go.qfdock,
            bdclosetab: go.bdclosetab,
            relative_splits: go.relative_splits,
            relative_docks: go.relative_docks,
            equalalways: go.equalalways,
            workspace_persist_unnamed: go.workspace_persist_unnamed,
        });
        // The per-workspace option overrides currently in effect, mirrored so `btv.wso`
        // reads the core's overlay (including overrides restored from the workspace shada).
        // Usually empty, so this is cheap and ungated.
        let wso: Vec<(String, OptionValue)> = self
            .editor
            .workspace_options()
            .iter()
            .map(|(name, scalar)| {
                let v = match scalar {
                    bemtvi_core::options::OptionScalar::Bool(b) => OptionValue::Bool(*b),
                    bemtvi_core::options::OptionScalar::Num(n) => OptionValue::Number(*n),
                    bemtvi_core::options::OptionScalar::Str(s) => OptionValue::String(s.clone()),
                };
                (name.clone(), v)
            })
            .collect();
        let _ = self.lua.set_wso_mirror(&wso);
        // The register file, mirrored so `vim.fn.getreg` / `getregtype` read the
        // core's current registers (stored cells + the read-only specials).
        //
        // Gated: this is O(stored bytes), and the stored bytes are whatever the user
        // last yanked — a `ggyG` over a real file makes an ungated push copy the
        // whole thing twice (once into Rust `String`s, once into Lua) on every
        // keystroke. The core's write counter covers the stored cells; the four
        // read-only specials (`%` `/` `:` `.`) resolve from live editor state and
        // move without it, so they are compared literally — they are short, and
        // comparing them is what keeps the unbounded half from being rebuilt.
        let reg_gen = self.editor.register_generation();
        let reg_specials = self.editor.register_specials();
        if self.reg_mirror_gen != Some(reg_gen) {
            // A register was written: re-push everything (which carries the specials).
            let regs = self.editor.register_mirror();
            let _ = self.lua.set_reg_mirror(&regs);
            self.reg_mirror_gen = Some(reg_gen);
            self.reg_mirror_specials = reg_specials;
        } else if self.reg_mirror_specials != reg_specials {
            // Only a special moved — `.` does so on every keystroke of an insert — so
            // refresh those four in place rather than re-copying the stored cells
            // along with them.
            let _ = self.lua.set_reg_specials(&reg_specials);
            self.reg_mirror_specials = reg_specials;
        }
        // Keyboard-macro state (`btv.macro.recording()` / `.executing()`): two
        // optional chars, gated on change so the common idle tick pays one compare.
        let macro_state = (
            self.editor.recording_register(),
            self.editor.executing_register(),
        );
        if self.macro_state_mirror != macro_state {
            let _ = self.lua.set_macro_state(macro_state.0, macro_state.1);
            self.macro_state_mirror = macro_state;
        }
        // The set marks (current buffer's locals + globals + numbered), mirrored so
        // `btv.mark.list` / the `marks` picker read the core's live positions — which
        // shift with edits and restore on undo. Small (a few dozen short rows).
        let marks = self.editor.marks_mirror();
        let _ = self.lua.set_marks_mirror(&marks);
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
        self.push_qflist_mirror();
        // Now that every mirror is consistent, fire the `nvim_buf_attach` callbacks.
        // `on_bytes` (and `on_reload`) go first — they carry the precise byte deltas a
        // consumer applies before the coarser line event — then `on_lines`, whose
        // callbacks read the refreshed buffer via
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

    /// Refresh the `btv._undotree` mirror that `vim.fn.undotree(bufnr)` reads.
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

    /// Refresh the `btv._qflist` mirror (`vim.fn.getqflist()`) from the editor's
    /// current quickfix list, plus the per-window `btv._loclist` mirror
    /// (`vim.fn.getloclist(win)`) from every window that has a location list.
    ///
    /// **Gated on a version**, because this runs on the per-keystroke mirror push
    /// and a list is not small: every entry costs a fresh Lua table plus 13 field
    /// sets, and a `:vimgrep` across a repo routinely produces thousands, which made
    /// typing 18x slower than with no list at all
    /// (`docs/plans/2026-08-08-per-keystroke-costs-round-2.md`). The version is the
    /// core's list-write counter paired with **which** windows currently hold a
    /// location list: the counter alone cannot see a loclist-owning window *close*
    /// (the list vanishes with the window without any stack being touched), and
    /// carrying the ids rather than a count means a close racing an open cannot
    /// alias. Computing it is O(windows) integer work.
    pub(crate) fn push_qflist_mirror(&mut self) {
        let version = (
            self.editor.qf_generation(),
            self.editor
                .window_ids()
                .into_iter()
                .filter(|w| self.editor.loclist(*w).is_some())
                .map(|w| w.0)
                .collect::<Vec<u64>>(),
        );
        if self.qf_mirror_version.as_ref() == Some(&version) {
            return;
        }
        self.qf_mirror_version = Some(version);
        let list = self.editor.qf_list();
        let items = qf_mirror_items(list);
        let title = list.title.clone();
        let _ = self.lua.set_qflist_mirror(&items, &title);
        // Rebuild the per-window location-list mirror from scratch so a window that
        // lost (or never had) a loclist drops out.
        let _ = self.lua.clear_loclist_mirror();
        for win in self.editor.window_ids() {
            if let Some(ll) = self.editor.loclist(win) {
                let items = qf_mirror_items(ll);
                let _ = self.lua.set_loclist_mirror(win.0, &items, &ll.title);
            }
        }
    }

    /// Absolutize a relative path against the session's effective working directory
    /// (the focused window's [`DirState`], which a remote `:cd` moves) — the rebase a
    /// daemon session needs before a path crosses the wire. One daemon process serves
    /// many sessions and keeps no per-session cwd, so a bare relative path shipped raw
    /// resolves against the daemon's launch dir and silently ignores `:cd`. The same
    /// rebase [`drain_pending_opens`](Self::drain_pending_opens) does for a relative
    /// `:edit`. An absolute path crosses unchanged. Only meaningful off-tick — a local
    /// session's process cwd already *is* the effective dir, so callers gate on
    /// [`remote_cwd_seeded`](EditHost::remote_cwd_seeded) — the daemon-fs gate, `false` for a
    /// serverless web (OPFS) session whose paths are root-relative and must not be rebased.
    fn abs_against_cwd(&self, path: &str) -> String {
        if std::path::Path::new(path).is_relative() {
            let win = self.editor.current_window_id();
            let tab = self.editor.current_tab_id();
            let (_, base) = self.dirs.effective(win, tab);
            base.join(path).to_string_lossy().into_owned()
        } else {
            path.to_string()
        }
    }

    /// Resolve a spawned child's working directory for a daemon session so `btv.run` /
    /// `vim.system` with no (or a relative) `cwd` runs where the session's cwd points —
    /// neovim's `vim.system` inherits the editor's cwd, but a bare daemon spawn would
    /// otherwise inherit the *daemon's* launch dir (the same stale-cwd class as a raw
    /// `btv.fs` path). A `None` cwd defaults to the effective dir; a relative one rebases;
    /// an absolute one (the common `cwd = vim.fn.getcwd()` a statusline passes) is kept.
    /// Off-tick + session-routed only — a local session's child already inherits the
    /// process cwd, and a `local` spawn (the plugin manager's git) runs on the local disk.
    #[cfg(feature = "native")]
    fn spawn_cwd(&self, cwd: Option<String>, local: bool) -> Option<String> {
        if local || !self.remote_cwd_seeded {
            return cwd;
        }
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, base) = self.dirs.effective(win, tab);
        Some(match cwd {
            Some(c) => self.abs_against_cwd(&c),
            None => base.to_string_lossy().into_owned(),
        })
    }

    /// Rebase every path an [`FsJob`] carries against the session cwd (see
    /// [`abs_against_cwd`](Self::abs_against_cwd)) so a relative `btv.fs.*` op resolves
    /// against the edit-host's `DirState` rather than the daemon's launch dir. Called on
    /// the outbound `btv.fs` path in a daemon session for a *session*-routed op (a `local`
    /// op runs on the local disk against the local process cwd, so it is left untouched).
    fn rebase_fs_job(&self, job: FsJob) -> FsJob {
        match job {
            FsJob::Stat { path } => FsJob::Stat {
                path: self.abs_against_cwd(&path),
            },
            FsJob::Lstat { path } => FsJob::Lstat {
                path: self.abs_against_cwd(&path),
            },
            FsJob::Exists { path } => FsJob::Exists {
                path: self.abs_against_cwd(&path),
            },
            FsJob::Readdir { path } => FsJob::Readdir {
                path: self.abs_against_cwd(&path),
            },
            FsJob::Read { path } => FsJob::Read {
                path: self.abs_against_cwd(&path),
            },
            FsJob::ReadText { path, encoding } => FsJob::ReadText {
                path: self.abs_against_cwd(&path),
                encoding,
            },
            FsJob::Write { path, data } => FsJob::Write {
                path: self.abs_against_cwd(&path),
                data,
            },
            FsJob::Append { path, data } => FsJob::Append {
                path: self.abs_against_cwd(&path),
                data,
            },
            FsJob::Mkdir {
                path,
                recursive,
                mode,
            } => FsJob::Mkdir {
                path: self.abs_against_cwd(&path),
                recursive,
                mode,
            },
            FsJob::Rename { from, to } => FsJob::Rename {
                from: self.abs_against_cwd(&from),
                to: self.abs_against_cwd(&to),
            },
            FsJob::Remove { path, recursive } => FsJob::Remove {
                path: self.abs_against_cwd(&path),
                recursive,
            },
            FsJob::Copy {
                src,
                dst,
                recursive,
            } => FsJob::Copy {
                src: self.abs_against_cwd(&src),
                dst: self.abs_against_cwd(&dst),
                recursive,
            },
            FsJob::Realpath { path } => FsJob::Realpath {
                path: self.abs_against_cwd(&path),
            },
            FsJob::HashFile { path, algo } => FsJob::HashFile {
                path: self.abs_against_cwd(&path),
                algo,
            },
            // `which` takes a NAME, not a path: a bare `gopls` must stay bare so the
            // executor searches `$PATH` for it (absolutizing it against the cwd would
            // turn the search into a lookup of one non-existent file). Only the
            // explicit-path form — anything containing a separator — is a real path
            // and gets the same cwd treatment as every other op.
            FsJob::Which { name } => FsJob::Which {
                name: if name.contains('/') {
                    self.abs_against_cwd(&name)
                } else {
                    name
                },
            },
        }
    }

    /// Arm the one-shot picker-spinner wake while a source run is in flight, so the
    /// prompt-row readout keeps animating through a long search — including one that
    /// streams nothing for seconds, which is exactly when the box would otherwise look
    /// frozen. Called from every frame: a search that ends leaves nothing armed, and a
    /// session with no picker open never arms it at all.
    ///
    /// Deliberately *not* `#[cfg(native)]` and routed through `apply_loop_op` — the
    /// wasm leg has its own timer wheel behind the same [`LoopOp`](bemtvi_lua::LoopOp),
    /// so one arm serves the local, daemon and browser sessions.
    pub(crate) fn arm_picker_spin_if_running(&mut self) {
        if self.editor.picker_running() && !self.picker_spin_armed {
            self.picker_spin_armed = true;
            self.apply_loop_op(bemtvi_lua::LoopOp::TimerStart {
                id: crate::PICKER_SPIN_TIMER_ID,
                delay_ms: crate::PICKER_SPIN_INTERVAL_MS,
                repeat_ms: 0,
            });
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
            // An awaited autocmd gate (`BufWritePre`) settled — queue the id for the
            // `run_pending` drain, which commits the parked write. Like `Schedule`, no
            // event loop is involved (the wait happened on the Lua promise), so it works
            // in every build.
            LoopOp::AuGateDone { id } => self.au_gate_done.push(id),
            // Timers / processes ride the tokio event loop. Native only for now; the
            // Worker-side timer wheel is slice 5d. (The internal per-buffer file watch
            // is armed directly from lifecycle, not via a `LoopOp`.)
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
                stream,
                local,
            } => self.fx.loop_command(LoopCommand::Spawn {
                id,
                argv: cmd,
                cwd: self.spawn_cwd(cwd, local),
                env,
                stdin,
                stream,
                local,
            }),
            #[cfg(feature = "native")]
            LoopOp::Kill { id } => self.fx.loop_command(LoopCommand::Kill { id }),
            // A duplex `btv.process` child (DAP / framed-protocol transport): spawn it
            // / feed its stdin through the actor. Its raw output returns on the
            // `loop_events` arm as `ProcOut`, its exit as `ProcExit`.
            #[cfg(feature = "native")]
            LoopOp::ProcOpen { id, cmd, cwd, env } => self.fx.loop_command(LoopCommand::ProcOpen {
                id,
                argv: cmd,
                cwd,
                env,
            }),
            #[cfg(feature = "native")]
            LoopOp::ProcWrite { id, data } => {
                self.fx.loop_command(LoopCommand::ProcWrite { id, data })
            }
            // The duplex child's `:kill()` — the actor's `Kill` handler terminates it
            // (duplex + one-shot share the actor's process map natively).
            #[cfg(feature = "native")]
            LoopOp::ProcClose { id } => self.fx.loop_command(LoopCommand::Kill { id }),
            // A `btv.socket` TCP connection (DAP `type="server"` transport): open / write
            // / close through the actor. Its `connected` / data / close return on the
            // `loop_events` arm.
            #[cfg(feature = "native")]
            LoopOp::SockConnect { id, host, port } => self
                .fx
                .loop_command(LoopCommand::SockConnect { id, host, port }),
            #[cfg(feature = "native")]
            LoopOp::SockWrite { id, data } => {
                self.fx.loop_command(LoopCommand::SockWrite { id, data })
            }
            #[cfg(feature = "native")]
            LoopOp::SockClose { id } => self.fx.loop_command(LoopCommand::SockClose { id }),
            // `btv.fs.watch` rides the actor's native watcher (inotify/FSEvents/kqueue),
            // coalesced there; the change events return on the `loop_events` arm.
            #[cfg(feature = "native")]
            LoopOp::FsWatch {
                id,
                path,
                recursive,
            } => self.fx.loop_command(LoopCommand::FsEventStart {
                id,
                path,
                recursive,
            }),
            #[cfg(feature = "native")]
            LoopOp::FsUnwatch { id } => self.fx.loop_command(LoopCommand::FsEventStop { id }),
            // An off-tick `btv.fs` op rides the actor's blocking pool against its
            // `LuaFs` clone (local syscalls, or a `RemoteLuaFs` wire round-trip in a
            // daemon session — now off the editor tick instead of inline). The typed
            // result returns on the `loop_events` arm as a `FsResult`.
            #[cfg(feature = "native")]
            LoopOp::Fs { id, job, local } => {
                // A session-routed op in a daemon session crosses to a stateless daemon,
                // so absolutize its relative path(s) against the edit-host's `DirState`
                // first (a `local` op runs on the local disk against the process cwd, so
                // it is left untouched).
                let job = if !local && self.remote_cwd_seeded {
                    self.rebase_fs_job(job)
                } else {
                    job
                };
                self.fx.loop_command(LoopCommand::Fs { id, job, local })
            }
            // `btv.git.*` — hand the whole job to the actor (`bemtvi_git::run_git_job` on the
            // blocking pool). No path rebase like `btv.fs`: the plugin callers pass absolute
            // paths (`btv.buf.name`), and the daemon-routed variant (which would need one)
            // lands in slice 1d. The result returns on `loop_events` as a `GitResult`.
            #[cfg(feature = "native")]
            LoopOp::Git { id, job, local } => {
                self.fx.loop_command(LoopCommand::Git { id, job, local })
            }
            // `btv.http.fetch` — hand the whole request to the actor (a local `ureq`
            // round-trip, or the daemon `http_op` leg). No rebase like `btv.fs`: the URL is
            // absolute, nothing is resolved against the cwd. `local` (`btv.http.fetch_local`)
            // forces the local `ureq` even in a daemon session.
            #[cfg(feature = "native")]
            LoopOp::Http { id, request, local } => {
                self.fx
                    .loop_command(LoopCommand::Http { id, request, local })
            }
            // `btv.http.mount` — publish a plugin's subroute on the editor's one listener.
            // The address comes from the OPTIONS, read here: the Lua bridge cannot see the
            // editor, and the actor must not (editor state is `!Send`). So this is the one
            // place `'httphost'`/`'httpport'` are turned into a concrete bind address —
            // which is also why they are inert until a plugin actually mounts.
            #[cfg(feature = "native")]
            LoopOp::HttpMount {
                id,
                name,
                timeout_ms,
            } => {
                let (host, port) = self.http_listen_addr();
                self.fx.loop_command(LoopCommand::HttpMount {
                    id,
                    name,
                    host,
                    port,
                    timeout: std::time::Duration::from_millis(timeout_ms),
                })
            }
            #[cfg(feature = "native")]
            LoopOp::HttpRespond { req_id, reply } => self
                .fx
                .loop_command(LoopCommand::HttpRespond { req_id, reply }),
            #[cfg(feature = "native")]
            LoopOp::HttpUnmount { id } => self.fx.loop_command(LoopCommand::HttpUnmount { id }),
            // The browser build has no tokio event loop; timers ride the Worker-side
            // wheel instead (slice 5d) — `vim.defer_fn` / `btv.timer` arm and fire there.
            #[cfg(not(feature = "native"))]
            LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            } => self.arm_wasm_timer(id, delay_ms, repeat_ms),
            #[cfg(not(feature = "native"))]
            LoopOp::TimerStop { id } => self.stop_wasm_timer(id),
            // Processes ride the daemon proc leg (Phase 6d) — the browser has no local
            // process to spawn, so a `vim.system` / `jobstart` is only possible against a
            // connected daemon. When one is wired, enqueue the spawn for the Worker to
            // forward over WebTransport (its pid/exit return inbound on `proc_spawned` /
            // `proc_exited`); serverless (no daemon) has no analogue, so fail *loud* in the
            // tick rather than silently dropping the request.
            #[cfg(not(feature = "native"))]
            LoopOp::Spawn {
                id,
                cmd,
                cwd,
                env,
                stdin,
                stream,
                local,
            } => {
                // A `local` spawn (the plugin manager's git) has nowhere to run on web — a
                // browser has no local process host, and routing it to the daemon would clone
                // on the *remote* (the bug this whole change fixes). Fail it LOUD instead: on
                // web, plugins arrive materialized via `config_bundle`, never git-cloned.
                if local {
                    let stderr = b"plugin sync (git) needs a local process host, which a \
                                   browser lacks \xE2\x80\x94 web plugins are fetched, not cloned"
                        .to_vec();
                    if let Err(e) = self.lua.run_callback(
                        id,
                        false,
                        CallbackArgs::Process {
                            code: -1,
                            stdout: Vec::new(),
                            stderr,
                        },
                    ) {
                        self.editor
                            .echo(format!("E5108: Error in vim.system on_exit: {e}"));
                    }
                    self.apply_lua_effects();
                } else if self.fx.has_remote_proc() {
                    self.fx.proc_spawn(id, cmd, cwd, env, stdin, stream);
                } else {
                    // No process host (serverless OPFS): a spawn has nowhere to run.
                    // Complete the caller's promise / stream LOUD with a spawn-failure
                    // exit (`code = -1`, the same shape a missing binary yields) instead
                    // of dropping the callback and leaving `btv.run` / `btv.run_stream`
                    // pending forever. Callers (e.g. the file picker) see the failure and
                    // fall back (an `btv.fs` walk). The `stderr` explains the cause.
                    let stderr = b"jobs/processes (vim.system / jobstart) require a \
                                   daemon \xE2\x80\x94 :connect to one"
                        .to_vec();
                    if let Err(e) = self.lua.run_callback(
                        id,
                        false,
                        CallbackArgs::Process {
                            code: -1,
                            stdout: Vec::new(),
                            stderr,
                        },
                    ) {
                        self.editor
                            .echo(format!("E5108: Error in vim.system on_exit: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // A kill only makes sense for a child the daemon is running; serverless never
            // spawned one, so it's a no-op there (nothing was enqueued).
            #[cfg(not(feature = "native"))]
            LoopOp::Kill { id } => {
                if self.fx.has_remote_proc() {
                    self.fx.proc_kill(id);
                }
            }
            // A duplex `btv.process` child has no wasm transport yet (the daemon proc
            // leg is one-shot `vim.system` only). Fail *loud* — fire the exit with
            // `code = -1` so the Lua handle's `on_exit` settles instead of hanging,
            // and surface the cause on its stderr stream — rather than silently
            // dropping the spawn.
            #[cfg(not(feature = "native"))]
            LoopOp::ProcOpen { id, cmd, cwd, env } => {
                if self.fx.has_remote_proc() {
                    self.fx.dproc_open(id, cmd, cwd, env);
                } else {
                    // Serverless (no daemon): a duplex child has nowhere to run. Settle
                    // the Lua handle LOUD (stderr + exit -1) instead of leaving it hung.
                    if let Err(e) = self.lua.run_process_recv(
                        id,
                        b"btv.process (duplex child) requires a daemon \xE2\x80\x94 :connect to one"
                            .to_vec(),
                        true,
                    ) {
                        self.editor
                            .echo(format!("E5108: Error in btv.process handler: {e}"));
                    }
                    if let Err(e) = self.lua.run_process_exit(id, -1) {
                        self.editor
                            .echo(format!("E5108: Error in btv.process on_exit: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            #[cfg(not(feature = "native"))]
            LoopOp::ProcWrite { id, data } => {
                if self.fx.has_remote_proc() {
                    self.fx.dproc_write(id, data);
                }
            }
            #[cfg(not(feature = "native"))]
            LoopOp::ProcClose { id } => {
                if self.fx.has_remote_proc() {
                    self.fx.dproc_kill(id);
                }
            }
            // `btv.socket` rides the daemon `sock_*` leg when a daemon is connected;
            // serverless fails the connect *loud* (settles on_close with an error).
            #[cfg(not(feature = "native"))]
            LoopOp::SockConnect { id, host, port } => {
                if self.fx.has_remote_proc() {
                    self.fx.sock_connect(id, host, port);
                } else {
                    if let Err(e) = self.lua.run_socket_closed(
                        id,
                        Some("btv.socket (TCP) requires a daemon — :connect to one".to_string()),
                    ) {
                        self.editor
                            .echo(format!("E5108: Error in btv.socket handler: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            #[cfg(not(feature = "native"))]
            LoopOp::SockWrite { id, data } => {
                if self.fx.has_remote_proc() {
                    self.fx.sock_write(id, data);
                }
            }
            #[cfg(not(feature = "native"))]
            LoopOp::SockClose { id } => {
                if self.fx.has_remote_proc() {
                    self.fx.sock_close(id);
                }
            }
            // `btv.fs.watch` streams over the daemon `luafs_watch` leg (Phase 3b) when a daemon is
            // connected — its recursive `notify` watcher pushes change batches back inbound on
            // `EditHost::fs_watch_event`. Serverless OPFS has NO change source (the tab is the sole
            // writer, and OPFS has no change-notification API), so there it fails the watch *loud*
            // — reject the stream's first pull — rather than arm a watch that silently never fires.
            #[cfg(not(feature = "native"))]
            LoopOp::FsWatch {
                id,
                path,
                recursive,
            } => {
                if self.fx.has_remote_proc() {
                    self.fx.fs_watch_stream(id, path, recursive);
                } else {
                    if let Err(e) = self.lua.run_fs_watch_event(
                        id,
                        Some(
                            "btv.fs.watch requires a daemon in this session \
                             (serverless OPFS has no filesystem change source)"
                                .to_string(),
                        ),
                        None,
                        Vec::new(),
                    ) {
                        self.editor
                            .echo(format!("E5108: Error in btv.fs.watch handler: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // Disarm the daemon watch (a no-op serverless, where nothing was armed).
            #[cfg(not(feature = "native"))]
            LoopOp::FsUnwatch { id } => {
                if self.fx.has_remote_proc() {
                    self.fx.fs_unwatch_stream(id);
                }
            }
            // The browser build has no event-loop actor; an off-tick `btv.fs` op is always
            // enqueued for the Worker to fulfill off-tick (its typed result returns inbound
            // on `EditHost::fs_op_result`). The Worker routes it to the daemon `luafs_op` leg
            // over WebTransport when connected (Phase 2), else to OPFS (Phase 3, serverless)
            // — the same daemon-or-OPFS split the off-tick `:e`/`:w` seam already uses. There
            // is always *some* fs on wasm (OPFS is the serverless fallback), so this never
            // needs the proc leg's "no host" loud reject, and never silently hits MEMFS.
            // On wasm the "local" store is OPFS: a `local`-flagged op (the plugin manager)
            // routes there instead of the daemon `luafs_op` leg (the Worker decides on the
            // `local` flag `fs_op` forwards). Plugin management stays local on web too.
            #[cfg(not(feature = "native"))]
            LoopOp::Fs { id, job, local } => {
                // A `?daemon=` web session routes a session (`!local`) op to the stateless
                // daemon over `luafs_op`, so absolutize its relative path(s) against `DirState`
                // first — the same rebase the native daemon arm does. A serverless / OPFS
                // session (`remote_cwd_seeded == false`) is NEVER rebased: OPFS is root-relative
                // and cwd-less, so its `.` must stay `.` (the Worker maps it to the OPFS root).
                // A `local` op (plugin manager) rides OPFS on web, so it is left untouched too.
                let job = if !local && self.remote_cwd_seeded {
                    self.rebase_fs_job(job)
                } else {
                    job
                };
                self.fx.fs_op(id, job, local)
            }
            // `btv.git.*` on wasm: hand the whole job to the Worker's daemon `git_op` leg.
            // Unlike `btv.fs` there is NO OPFS fallback — there is no in-browser git engine —
            // so a serverless session (no daemon) rejects the op LOUD in the tick (the
            // `FsWatch` precedent), never a silent empty result. A `?daemon=` web session
            // routes it over the wire, where the daemon runs `run_git_job` against the real
            // repo. No path rebase: the plugin callers pass absolute paths.
            #[cfg(not(feature = "native"))]
            LoopOp::Git { id, job, local } => {
                if self.fx.has_remote_fs() {
                    self.fx.git_op(id, job, local);
                } else {
                    let result = Err(bemtvi_lua::GitError {
                        code: "ENODAEMON".to_string(),
                        message: "btv.git requires a daemon in this session \
                                  (a serverless web session has no git engine)"
                            .to_string(),
                    });
                    if let Err(e) =
                        self.lua
                            .run_callback(id, false, CallbackArgs::GitResult { result })
                    {
                        self.editor
                            .echo(format!("E5108: Error in btv.git handler: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // `btv.http.fetch` on wasm: hand the request to the Worker (the daemon `http_op`
            // leg when connected, else the browser's own `fetch()`). No host gate — the
            // browser always has `fetch()`, so a serverless session runs HTTP directly.
            // `local` (`btv.http.fetch_local`) forces the browser `fetch()`, bypassing the daemon.
            #[cfg(not(feature = "native"))]
            LoopOp::Http { id, request, local } => self.fx.http_op(id, request, local),
            // `btv.http.mount` on wasm: a browser tab cannot bind a TCP port, and unlike
            // `fetch()` there is no universal fallback to degrade to — so this fails LOUD
            // (the `LoopOp::Spawn` / `FsWatch` precedent) rather than handing back a URL
            // that would 404. Phase 3 of the mount plan gives the web build real parity via
            // a Service Worker on the page's own origin, which satisfies this same
            // `HttpServerRequest`/`HttpServerReply` contract.
            // `btv.http.mount` on wasm: a tab cannot bind a port, so the Worker registers a
            // Service Worker that intercepts `/plugin/*` on the page's own origin and relays
            // each request back in — the same contract as the native listener. No host gate:
            // a Service Worker needs no daemon, only a secure origin (and an insecure one
            // rejects the mount, inbound, rather than handing back a URL that would 404).
            //
            // The name→id route table is kept HERE rather than in JS so the browser and the
            // native listener resolve a mount the same way.
            #[cfg(not(feature = "native"))]
            LoopOp::HttpMount { id, name, .. } => {
                self.http_routes.insert(name.clone(), id);
                self.fx.http_mount(id, name);
            }
            #[cfg(not(feature = "native"))]
            LoopOp::HttpRespond { req_id, reply } => self.fx.http_respond(req_id, reply),
            #[cfg(not(feature = "native"))]
            LoopOp::HttpUnmount { id } => {
                self.http_routes.retain(|_, mount| *mount != id);
                self.fx.http_unmount(id);
            }
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
            LoopEvent::ProcessStdout { id, lines } => {
                // A streaming child (`btv.run_stream`) emitted a batch of stdout
                // lines: fire the persistent stdout handler, then drain whatever it
                // queued (a picker source's `ctx.push` of new candidates).
                if let Err(e) = self.lua.run_process_stdout(id, lines) {
                    self.editor
                        .echo(format!("E5108: Error in btv.run_stream handler: {e}"));
                }
                self.apply_lua_effects();
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
            LoopEvent::ProcOut { id, data, stderr } => {
                // A duplex `btv.process` child (DAP transport) emitted a raw chunk:
                // hand it to the persistent receiver, then drain whatever the Lua
                // framing/dispatch queued (breakpoint signs, view renders, …).
                if let Err(e) = self.lua.run_process_recv(id, data, stderr) {
                    self.editor
                        .echo(format!("E5108: Error in btv.process handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::ProcExit { id, code } => {
                if let Err(e) = self.lua.run_process_exit(id, code) {
                    self.editor
                        .echo(format!("E5108: Error in btv.process on_exit: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::SockConnected { id } => {
                if let Err(e) = self.lua.run_socket_connected(id) {
                    self.editor
                        .echo(format!("E5108: Error in btv.socket on_connect: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::SockData { id, data } => {
                if let Err(e) = self.lua.run_socket_data(id, data) {
                    self.editor
                        .echo(format!("E5108: Error in btv.socket handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::SockClosed { id, error } => {
                if let Err(e) = self.lua.run_socket_closed(id, error) {
                    self.editor
                        .echo(format!("E5108: Error in btv.socket on_close: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::FsEvent {
                id,
                error,
                kind,
                paths,
            } if id < crate::INTERNAL_WATCH_BASE => {
                // A Lua `btv.fs.watch` change (id below the internal-watch base): fire
                // the watch's stream pump with the coalesced `{ kind, paths }` batch,
                // or its terminal `error`. Effects the handler queues drain right
                // after, like the process-event arms.
                // A terminal arm failure (bad path / watch limit) ends the stream, so
                // the watch is dead for good. Forget it on the event-loop actor — the
                // daemon link's `armed` map and the local watcher table — or every
                // reconnect's `rearm_all` would re-arm the dead watch and push the
                // error again, once per re-dial, forever. A fresh `btv.fs.watch` mints
                // a new id and arms clean; a no-op when the arm never reached the
                // actor (a local arm failure inserts no watcher).
                let terminal = error.is_some();
                if let Err(e) = self.lua.run_fs_watch_event(id, error, kind, paths) {
                    self.editor
                        .echo(format!("E5108: Error in btv.fs.watch handler: {e}"));
                }
                if terminal {
                    self.apply_loop_op(LoopOp::FsUnwatch { id });
                }
                self.apply_lua_effects();
            }
            LoopEvent::FsEvent { id, error, .. } => {
                // An internal per-buffer file watch's auto-trigger (id ≥ BASE; the Lua
                // `vim.uv.fs_event` surface is gone — `btv.fs.watch` is handled above).
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
            LoopEvent::FsResult { id, result } => {
                // A workspace edit's own file operation (`rename`/`delete`) rides the
                // same seam under an id above `WORKSPACE_FS_JOB_BASE`, and settles in
                // the editor rather than in a Lua promise.
                if self.on_workspace_fs_result(id, &result) {
                    self.apply_lua_effects();
                    return;
                }
                // An off-tick `btv.fs` op settled: resolve / reject its promise on this
                // thread (the typed result is marshalled to Lua in `run_callback`),
                // then drain whatever the reaction queued — the process-event shape.
                if let Err(e) = self
                    .lua
                    .run_callback(id, false, CallbackArgs::FsResult { result })
                {
                    self.editor
                        .echo(format!("E5108: Error in btv.fs handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::GitResult { id, result } => {
                // An off-tick `btv.git` op settled: resolve / reject its promise on this
                // thread (marshalled to Lua in `run_callback`), then drain the reaction —
                // the `FsResult` shape.
                if let Err(e) = self
                    .lua
                    .run_callback(id, false, CallbackArgs::GitResult { result })
                {
                    self.editor
                        .echo(format!("E5108: Error in btv.git handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::HttpMountResult { id, result } => {
                // `btv.http.mount` settled: resolve with the bound origin, or reject.
                if result.is_ok() {
                    // The listener is up (this mount bound it, or an earlier one did).
                    // Record the options it bound under: this is both the gate and the
                    // baseline for the per-tick option check. A failed mount sets nothing —
                    // it opened nothing. (The origin itself is mirrored into Lua by the
                    // resolve, so it is not duplicated here.)
                    self.http_serving = Some(self.http_listen_addr());
                }
                let result = result.map_err(|message| bemtvi_lua::HttpMountError { message });
                if let Err(e) =
                    self.lua
                        .run_callback(id, false, CallbackArgs::HttpMountResult { result })
                {
                    self.editor
                        .echo(format!("E5108: Error in btv.http.mount handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::HttpServerRequest {
                id,
                req_id,
                request,
            } => {
                // An inbound request for a mount: run the plugin's `on_request` on this
                // thread. Its `respond` queues a `LoopOp::HttpRespond` that
                // `apply_lua_effects` hands back to the parked axum handler.
                if let Err(e) = self.lua.run_http_server_request(id, req_id, request) {
                    self.editor
                        .echo(format!("E5108: Error in btv.http.mount handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::HttpRebound { origin, host, port } => {
                // The listener moved; every mount is still live. Record what actually bound
                // (as reported, not as currently configured — a second `:set` may already be
                // in flight) and update the one place `Mount:origin()` reads.
                self.http_serving = Some((host.clone(), port));
                if self.http_rebind_inflight == Some((host, port)) {
                    self.http_rebind_inflight = None;
                }
                if let Err(e) = self.lua.run_http_rebound(&origin) {
                    self.editor
                        .echo(format!("E5108: Error in btv.http.mount handler: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::HttpRebindErr {
                message,
                host,
                port,
            } => {
                // The rebind failed and nothing moved — the old listener still serves. Put
                // the option back, so `:set httpport?` can never disagree with the live
                // address, and say so: a silently ignored `:set` is exactly the lie the
                // fail-loud rule exists to prevent.
                if self.http_rebind_inflight == Some((host.clone(), port)) {
                    self.http_rebind_inflight = None;
                }
                // Only revert if the failed address is still what the options say — the user
                // may have already moved on to a third value, whose own rebind is in flight
                // and must not be clobbered by this older failure.
                let stale = (
                    self.editor.options.httphost.clone(),
                    self.editor.options.httpport,
                ) != (host, port);
                if !stale {
                    if let Some((good_host, good_port)) = self.http_serving.clone() {
                        self.editor.options.httphost = good_host;
                        self.editor.options.httpport = good_port;
                    }
                }
                self.editor.echo(format!("E5109: {message}"));
                self.apply_lua_effects();
            }
            LoopEvent::HttpResult { id, result } => {
                // An off-tick `btv.http.fetch` settled: resolve / reject its promise on this
                // thread (the typed response is marshalled to Lua in `run_callback`), then
                // drain whatever the reaction queued.
                if let Err(e) =
                    self.lua
                        .run_callback(id, false, CallbackArgs::HttpResult { result })
                {
                    self.editor
                        .echo(format!("E5108: Error in btv.http handler: {e}"));
                }
                self.apply_lua_effects();
            }
        }
    }

    /// Dispatch the registered `btv.decor` providers for one window whose visible
    /// range changed. Builds the `ctx` snapshot the provider sees — the visible line
    /// slice (read directly from the rope, not the whole buffer) and the buffer
    /// filetype (for the provider's `bufs` filter) — then hands it to Lua with the
    /// viewport `generation` core stamped, so a publish the provider produces can be
    /// gen-gated at apply time (Phase 3). A throwing provider is isolated Lua-side and
    /// surfaced as `E5108` here. Phase 2 of `btv.decor`.
    fn dispatch_decor(&mut self, vp: DecorViewport) {
        let (lines, bot) = {
            let Some(buf) = self.editor.buffer_of(vp.buf) else {
                return;
            };
            // Re-clamp `bot` to the live buffer: the snapshot was taken when the dirty
            // entry was queued; an edit since could have shortened the buffer.
            let bot = vp.bot.min(buf.line_count().saturating_sub(1));
            let mut lines = Vec::with_capacity(bot.saturating_sub(vp.top) + 1);
            for row in vp.top..=bot {
                lines.push(buf.line(row));
            }
            (lines, bot)
        };
        let filetype = self.editor.buffer_filetype(vp.buf).unwrap_or_default();
        let buftype = self.editor.buffer_buftype(vp.buf);
        if let Err(e) = self.lua.run_decor_dispatch(
            vp.win.0,
            vp.buf.0,
            vp.top,
            bot,
            vp.generation,
            &filetype,
            buftype,
            &lines,
        ) {
            self.editor
                .echo(format!("E5108: Error in btv.decor provider: {e}"));
        }
    }

    /// Push the **`KeyPending`** event to `btv.on_key_pending` listeners when the live
    /// pending key-context differs from the last one pushed — the which-key / showcmd
    /// oracle (the design's "fires whenever the pending key-context changes"). Gated
    /// on a registered listener so a no-which-key config never walks the trie or
    /// re-enters Lua. The change-detection (`last_key_pending`) is what keeps the
    /// event fire-on-change rather than per keystroke: an unchanged context (a key
    /// that didn't move the prefix, an off-tick timer) is a no-op, and the prefix
    /// clearing fires exactly one *cleared* event (`keys = ""`). A handler that opens
    /// a float queues an effect applied here; deferred work re-loops the fixpoint.
    fn emit_key_pending(&mut self) {
        if !self.lua.has_key_pending_listeners() {
            return;
        }
        // The scope a grabbing widget owns input in (its keymap bucket), else the
        // buffer's editing mode — so a withheld widget prefix lists *that widget's*
        // keys (source C), mirroring how `feed_matcher` picks the match scope.
        let scope = match crate::keymap::widget_bucket(self.editor.key_context()) {
            Some(bucket) => crate::keymap::MatchScope::Widget(bucket),
            None => crate::keymap::MatchScope::Editing(self.editor.mode),
        };
        // Source A/C: the matcher's withheld mapped prefix. Then **source B** — the
        // built-in command grammar — joins it (editing scopes only; a widget has no
        // core grammar). When the matcher withholds nothing, source B *is* the context
        // (`f` find-char, `z`, `<C-w>`, operator-pending — reached the editor and left
        // it mid-command). When it withholds a mapped prefix that is *also* a built-in
        // prefix — `g`, withheld by the LSP `gd`/`gD`/`gr` defaults — the built-in's
        // enumerated continuations (`gg`/`gt`/…) are merged into the withheld context,
        // since the matcher can't see them: the key never reached the grammar. Open
        // built-in leaves carry a `label` and no continuations (a hint card); the
        // finite prefixes (`g`/`z`/`<C-w>`) carry an enumerated list (Phase 2).
        let mut ctx = self.keymaps.pending_context(scope);
        if let crate::keymap::MatchScope::Editing(mode) = scope {
            match &mut ctx {
                // Withheld mapped prefix live: merge in any built-in continuations the
                // same key run would lead to (folded hypothetically — the key has not
                // reached the editor). A leader like `<Space>` folds to a complete
                // motion, so `command_pending_after` returns `None` and nothing merges.
                Some(kp) => {
                    // Merge the built-in vim grammar's continuations for a mapped prefix
                    // that is *also* a built-in (`g`, withheld by the LSP `gd`/… maps) —
                    // but NOT in Helix: the vim `g`/`z`/… grammar doesn't describe Helix,
                    // so the Helix `g`/`<Space>` menus are fully defined by their trie
                    // maps (source A) and must not gain stray vim rows (`gj`/`g#`/tabs).
                    if !mode.is_helix() {
                        if let Some(cp) = command_pending_after(mode, &parse_keys(&kp.keys)) {
                            merge_builtin_continuations(kp, &cp.continuations);
                        }
                    }
                }
                // Nothing withheld: source B is the whole context. The built-in
                // continuations are available; any mapped continuation that *shares*
                // this prefix (the LSP `g` defaults under a `g` that just timed out into
                // the built-in grammar) is surfaced too, flagged unavailable — kept
                // visible so the popup doesn't drop rows the user couldn't read yet.
                None => {
                    // Source B: the built-in grammar mid-command. In Helix the native
                    // sub-grammars (`m`/`z`/`f`/`"`/`r`) live outside the vim
                    // `PendingCommand`, so ask the Helix projection instead.
                    let pending = if mode.is_helix() {
                        self.editor.helix_command_pending()
                    } else {
                        self.editor.command_pending()
                    };
                    if let Some(cp) = pending {
                        let mut continuations: Vec<crate::keymap::Continuation> = cp
                            .continuations
                            .iter()
                            .map(builtin_to_continuation)
                            .collect();
                        let have: HashSet<String> =
                            continuations.iter().map(|c| c.key.clone()).collect();
                        for mut stale in self.keymaps.continuations_at(scope, &parse_keys(&cp.keys))
                        {
                            if !have.contains(&stale.key) {
                                stale.available = false;
                                continuations.push(stale);
                            }
                        }
                        // Match the source-A contract: continuations sorted by key so
                        // the event payload is deterministic.
                        continuations.sort_by(|a, b| a.key.cmp(&b.key));
                        ctx = Some(crate::keymap::KeyPending {
                            mode: scope.mode_code().to_string(),
                            keys: cp.keys,
                            continuations,
                            label: Some(cp.label.to_string()),
                        });
                    }
                }
            }
        }
        if ctx == self.last_key_pending {
            return;
        }
        self.last_key_pending = ctx.clone();
        // A cleared context (no withheld prefix and no pending command) is pushed as
        // `keys = ""` with no continuations / label, in the scope it cleared in — a
        // which-key popup reads that as "close". A live context carries its prefix and
        // either continuations (A/C) or a label (B).
        let (mode, keys, conts, label) = match &ctx {
            Some(kp) => {
                let conts: Vec<(&str, Option<&str>, &str, bool)> = kp
                    .continuations
                    .iter()
                    .map(|c| {
                        (
                            c.key.as_str(),
                            c.desc.as_deref(),
                            c.kind.as_str(),
                            c.available,
                        )
                    })
                    .collect();
                (
                    kp.mode.as_str(),
                    kp.keys.as_str(),
                    conts,
                    kp.label.as_deref(),
                )
            }
            None => (scope.mode_code(), "", Vec::new(), None),
        };
        if let Err(e) = self.lua.run_key_pending(mode, keys, &conts, label) {
            self.editor
                .echo(format!("E5108: Error in btv.on_key_pending handler: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Lower one [`DecorPublish`] into the extmark layer (Phase 3 of `btv.decor`).
    /// First the **stale-drop**: if the window's viewport generation has moved on
    /// since this batch was dispatched (a newer scroll superseded it), painting it
    /// would show marks for a range the user already left, so it is dropped before
    /// any mutation (Decision 4). Otherwise the provider's namespace is cleared on the
    /// buffer and the fresh batch set into it — a republish replaces the prior
    /// viewport's marks wholesale (Decision 3). Ids restart at `1` each publish (the
    /// namespace is empty after the clear); a mark without a `priority` takes the
    /// default extmark priority, so it paints over treesitter/semantic spans.
    fn apply_decor_publish(&mut self, publish: DecorPublish) {
        if publish.gen != self.editor.decor_generation(WindowId(publish.win)) {
            return;
        }
        // Mark the provider's namespace ephemeral so undo/redo carry its live marks
        // across a snapshot restore rather than swapping them out (otherwise undoing to
        // a pre-provider state — the root node — flashes the decorations). Idempotent.
        self.editor.mark_extmark_namespace_ephemeral(publish.ns);
        self.apply_extmark_op(ExtmarkOp::Clear {
            bufnr: publish.buf,
            ns: publish.ns,
            line_start: 0,
            line_end: -1,
        });
        for (i, mark) in publish.marks.into_iter().enumerate() {
            self.apply_extmark_op(ExtmarkOp::Set {
                bufnr: publish.buf,
                ns: publish.ns,
                id: i as u64 + 1,
                row: mark.row,
                col: mark.col,
                end_row: mark.end_row,
                end_col: mark.end_col,
                hl_group: mark.hl_group,
                priority: mark.priority.unwrap_or(bemtvi_core::DEFAULT_PRIORITY),
                // A provider's marks ARE extmarks: the whole decoration payload —
                // virtual text/lines, a gutter sign, a line background — rides through
                // exactly as it would from `btv.buf.set_extmark`.
                decor: mark.decor,
                right_gravity: mark.right_gravity,
                end_right_gravity: mark.end_right_gravity,
            });
        }
    }

    /// The settle contract for an off-tick event arm: drive every queued effect to
    /// convergence (`run_pending`, which also drains `self.scheduled`) and repaint
    /// once. `dirty` forces a repaint even when no Lua callback ran (e.g. an LSP
    /// event that only updated cached state); a callback that queued work always
    /// repaints. Factored out so the syntax/LSP/loop arms share one tail and no
    /// off-tick callback's deferred `vim.cmd` is left undriven.
    /// The address `btv.http.mount` should listen on — the `'httphost'` / `'httpport'`
    /// options, read live off the editor. This is the *only* place those options are turned
    /// into a bind address: the Lua bridge cannot see the editor, and the event-loop actor
    /// must not (editor state is `!Send`), so both are handed a concrete address from here.
    #[cfg(feature = "native")]
    fn http_listen_addr(&self) -> (String, u16) {
        (
            self.editor.options.httphost.clone(),
            self.editor.options.httpport,
        )
    }

    /// Notice an `'httphost'` / `'httpport'` write while mounts are serving, and move the
    /// listener to match.
    ///
    /// A `:set httpport=9000` must not silently do nothing: the listener lives for the
    /// session, so "takes effect at the next bind" would be a lie — there is no next bind.
    /// The actor binds the new address before dropping the old, so a failure changes nothing
    /// and comes back as [`LoopEvent::HttpRebindErr`], which reverts the option.
    ///
    /// There is no `OptionSet` event in the tree, so this compares on the tick. Gated on a
    /// listener actually existing — a config with no HTTP plugin never gets past the first
    /// line. Called from [`run_pending`](Self::run_pending), which every input / ex-command
    /// / callback path funnels through (a `:set` never touches Lua, so an
    /// `apply_lua_effects` hook would miss it).
    #[cfg(feature = "native")]
    fn sync_http_listen_addr(&mut self) {
        let Some(serving) = self.http_serving.clone() else {
            return; // nothing bound — the options stay inert until a plugin mounts
        };
        let want = self.http_listen_addr();
        if want == serving {
            return;
        }
        // Already asked for exactly this — don't re-send it every tick while the actor
        // works. `http_serving` stays the last-GOOD address until `HttpRebound` confirms the
        // move, so that a failure has something true to revert the option to.
        if self.http_rebind_inflight.as_ref() == Some(&want) {
            return;
        }
        self.http_rebind_inflight = Some(want.clone());
        self.fx.loop_command(LoopCommand::HttpRebind {
            host: want.0,
            port: want.1,
        });
    }

    pub(crate) fn settle_events(&mut self, dirty: bool) {
        // Hold a restored session's focus through startup: re-pin the layer it was quit from
        // BEFORE this settle's repaint, so a sidebar plugin's async dock-(re)build — which
        // can grab focus several ticks in — never shows. Cheap no-op once focus already sits
        // where the restore wanted it; the first real key / mouse releases the hold (see
        // `btv_input` in dispatch). Done here, the shared internal-event settle, so it covers
        // every async source (timers, the tree's `wait_for` build, LSP, watches).
        self.editor.finalize_session_focus();
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
    /// Refresh the **completion docs float** beside the popup — the doc-float-window
    /// replacement for the old `menu.docs` overlay. Sources the selected row's docs
    /// markdown (inline plugin `doc`, a plugin `resolve`'s cached docs, or an `lsp`
    /// row's `detail` + `documentation`) and hands it to `open_completion_docs_float`,
    /// which owns the placement + rendering; passes the configured `docs_wrap`. Closes
    /// the float when no popup is open, docs are disabled, or the row has none. Called
    /// once per settle from [`run_pending`](Self::run_pending) after the selection /
    /// resolve is final — so it also updates when an async `completionItem/resolve` or
    /// a plugin `resolve` reply lands (each re-settles).
    pub(crate) fn sync_complete_docs_float(&mut self) {
        if !self.editor.completion_active() || !self.editor.complete_docs_enabled() {
            self.editor.close_completion_docs_float();
            return;
        }
        let sections = self.selected_complete_docs_sections();
        if sections.is_empty() {
            self.editor.close_completion_docs_float();
            return;
        }
        let wrap = self.editor.complete_docs_wrap();
        self.editor.open_completion_docs_float(&sections, wrap);
    }

    /// The labelled markdown sections [`sync_complete_docs_float`](Self::sync_complete_docs_float)
    /// renders for the actively-selected completion row, from whichever of the three
    /// docs sources fits it: a plugin async row's **inline** `doc`; else a plugin row's
    /// **resolve** handle whose docs the server fetched lazily into
    /// `complete_resolve_docs`; else an `lsp` row whose contributors' `detail` +
    /// `documentation` live in the server's LSP item cache. Empty for a noselect popup,
    /// a `buffer` row, or a row whose lazy docs haven't landed yet.
    ///
    /// Only the `lsp` source has more than one thing to say about a row (a symbol two
    /// servers both offer); the plugin sources produce a single, unlabelled section,
    /// which renders exactly as it did before sections existed.
    fn selected_complete_docs_sections(&self) -> Vec<(String, String)> {
        let bare = |doc: String| vec![(String::new(), doc)];
        // Inline docs (a plugin source's `push { doc = … }`).
        if let Some(doc) = self.editor.complete_selected_doc() {
            return bare(doc);
        }
        // A plugin `resolve` handle → the server-fetched docs cache. On the wasm edit-host
        // the plugin resolve path (`complete_plugin_maybe_resolve`) never runs, so the
        // cache is always empty there and this simply yields nothing.
        if let Some(id) = self.editor.complete_selected_resolve() {
            return self
                .complete_resolve_docs
                .get(&id)
                .cloned()
                .map(bare)
                .unwrap_or_default();
        }
        // An `lsp` row → the server's LSP item cache, one section per server that
        // offered it. LSP completion runs on both builds (native locally, wasm over the
        // daemon), so this is not native-gated — the web python demo shows docs too.
        if let Some((key, true)) = self.editor.complete_selected() {
            return self.lsp_complete_docs_sections(key);
        }
        Vec::new()
    }

    /// Refresh the **cmdline wildmenu docs** float beside the wildmenu box — the
    /// doc-float-window replacement for the old `menu.docs` overlay on the cmdline path.
    /// Plain help text (no markdown render): the highlighted catalog row's `doc`,
    /// word-wrapped to the resolved width and bottom-aligned to the box (so it abuts the
    /// command line alongside it), exactly as `project_cmdline_docs` placed it. Closes
    /// the float when no wildmenu is open, docs are disabled, or no row is selected.
    /// Called once per settle from [`run_pending`](Self::run_pending).
    pub(crate) fn sync_cmdline_docs_float(&mut self) {
        if !self.editor.cmdline_complete_active() || !self.editor.cmdline_complete_docs() {
            self.editor.close_cmdline_docs_float();
            return;
        }
        let Some(doc) = self.editor.cmdline_selected_doc() else {
            self.editor.close_cmdline_docs_float();
            return;
        };
        let lines: Vec<String> = doc
            .lines()
            .map(str::to_string)
            .skip_while(|l| l.trim().is_empty())
            .collect();
        // All in windows-area cells (the frame the `FloatRelative::Editor` docs float
        // lays out in): `box_row`/`box_col` are the wildmenu box's top-left, `bound_w`
        // the editor width the sidebar is bounded by.
        let Some((box_row, box_col, box_w, box_h, bound_w)) = self.editor.cmdline_menu_box() else {
            self.editor.close_cmdline_docs_float();
            return;
        };
        if lines.is_empty() {
            self.editor.close_cmdline_docs_float();
            return;
        }
        /// Cap the docs float's content width / height (the wildmenu help preview).
        const MAX_DOCS_W: usize = 60;
        const MAX_DOCS_H: usize = 12;
        let content_w = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_DOCS_W);
        // Beside the box, bounded by the editor (the command line spans its full width),
        // flipping to the side with more room — or nothing rather than a sliver.
        let Some((docs_col, docs_w)) =
            crate::redraw::place_docs_beside(box_col, box_w, content_w, bound_w)
        else {
            self.editor.close_cmdline_docs_float();
            return;
        };
        // Word-wrap to the resolved width (the window carries the pre-wrapped lines, so
        // its own `wrap` stays off — the lines already fit) and bottom-align to the box:
        // the content bottom lands on the box's content bottom (`box_row + box_h`).
        let wrapped = crate::redraw::wrap_doc_lines(&lines, docs_w);
        let docs_h = wrapped
            .len()
            .min(MAX_DOCS_H)
            .min((box_row + box_h).saturating_sub(1).max(1));
        let docs_row = (box_row + box_h).saturating_sub(docs_h + 1);
        // The float's OUTER box (border included) sits one row/col out so the content
        // lands at `(docs_row, docs_col)` — flush against the box, the same cells the old
        // bordered overlay drew its content in.
        self.editor.open_cmdline_docs_float(
            wrapped,
            docs_row.saturating_sub(1),
            docs_col.saturating_sub(1),
            docs_w as u16,
            docs_h,
            false,
        );
    }

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
        // …and the current-buffer snapshot, so a command body reading `expand("%")` (a
        // `:BtvDiffGit`-style command right after `:edit`) sees the current buffer's name.
        self.refresh_cur_buf_snapshot();
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
            // A `:wqa` / `:xa` quit deferred this convergence: install its gate *before*
            // the pre-write drain, so a synchronous commit below advances it against a
            // live set (off-tick acks advance it later). Inside the loop so a `:wqa` run
            // from a queued command / callback still installs its gate the same pass.
            self.drain_pending_quit_all();
            // Async `BufWritePre` gates that settled since the last round: commit each
            // parked write now that its handler promises have all resolved. Ordered first
            // so the commit's completed-write (BufWritePost) is picked up by
            // `drain_write_events` below in the same round.
            self.drain_au_gate_done();
            // The gated editor-exit sequence (`QuitPre` → `ExitPre` → `VimLeavePre` →
            // `VimLeave` + `should_quit`): begin it when core committed a quit this
            // convergence, and advance it as each gated handler settles (an async
            // `ExitPre`/`VimLeavePre` resumes here once `drain_au_gate_done` above cleared its
            // `exit_gate`). Ordered right after the gate drain so a same-tick settle advances
            // the sequence in the same round. Sets `should_quit` at the end — the run loop's
            // quit funnel breaks on it.
            self.drive_exit();
            // Pre-write intents recorded this convergence (`:w` / `:wq`): fire (and
            // await) `BufWritePre` before committing each write, so a handler's buffer
            // mutation is what lands on disk. A write whose handlers settle synchronously
            // commits here; one with a pending async handler is parked and committed by
            // `drain_au_gate_done` above on a later round. Ordered *before*
            // `drain_write_events` so a synchronous commit's `BufWritePost` fires this
            // same round.
            self.drain_pre_writes();
            // Writes completed this convergence (a committed `:w` / `:wall`, or a
            // finalized off-tick save): fire each one's `BufWritePost` (and `BufWritePre`
            // for the not-yet-fired off-tick ack). Inside the fixpoint so a handler's
            // queued `vim.cmd`/`:lua` drains in the same convergence, and a handler that
            // itself writes (`vim.cmd('w')`) keeps the loop going via the
            // `has_pending_pre_writes`/`has_write_events` break checks below.
            self.drain_write_events();
            // `<C-w>d` / `<C-w><C-d>` (neovim's built-in "show diagnostics under the
            // cursor"): core recorded the chord on the keystroke; open the float here
            // — the diagnostic store lives behind the server seam, so core can't. The
            // same surface `btv.diagnostic.open_float()` uses; a clean line is a loud
            // no-op (an echoed message). One-shot: `take_diagnostic_float` clears it.
            if self.editor.take_diagnostic_float() {
                self.diagnostics_open_float();
            }
            // A completion row whose accept was **delegated** (the built-in `lsp`
            // source): core recorded the chosen row's key on the keystroke; apply its
            // `textEdit` + `additionalTextEdits` here, which core can't (LSP/encoding-
            // aware edits). Drained in `run_pending` (not `apply_lua_effects`) because
            // the accept keystroke queues no Lua, so `apply_lua_effects` may not run —
            // but `run_pending` always does, once, after every key.
            // Route the delegated key by its range (highest base first): a **plugin**
            // `on_accept` row (`PLUGIN_ACCEPT_KEY_BASE`) runs its Lua callback; a
            // `snippets`-source row (`SNIPPET_COMPLETE_KEY_BASE`, feature-agnostic —
            // the engine is in core) expands its body into the tabstop session; a bare
            // key is an `lsp` row whose `textEdit` the LSP applier applies.
            if let Some(key) = self.editor.complete_accept_request {
                self.editor.complete_accept_request.take();
                if key >= crate::snippet::PLUGIN_ACCEPT_KEY_BASE {
                    // NOT `native`-gated: `complete_plugin_accept` only touches the editor
                    // and the Lua runtime, both of which the wasm edit-host has. Gating it
                    // made the browser build take this branch and then do NOTHING — the
                    // accept was consumed, the popup closed, and the callback never ran, so
                    // every `btv.complete.source` item carrying an `on_accept` (the whole
                    // mechanism bemtvi-snippets expands through) silently inserted nothing.
                    self.complete_plugin_accept(key - crate::snippet::PLUGIN_ACCEPT_KEY_BASE);
                } else if key >= crate::snippet::SNIPPET_COMPLETE_KEY_BASE {
                    self.complete_snippet_accept(key - crate::snippet::SNIPPET_COMPLETE_KEY_BASE);
                } else {
                    self.complete_lsp_accept(key);
                }
            }
            // The docs sidebar's lazy-docs fetch (Phase 4-D): when the highlighted
            // `lsp` row has unresolved docs, issue a `completionItem/resolve`. Like the
            // accept drain, this runs once per key (the guard skips while in flight), so
            // the sidebar fills in shortly after the user lands on a row.
            self.complete_lsp_maybe_resolve();
            // The same lazy-docs fetch for a **plugin** row (`btv.complete.source`'s
            // `resolve` callback) — ask Lua to resolve the highlighted row's docs if
            // it carries a resolve handle and they aren't cached yet (Phase 4-E).
            #[cfg(feature = "native")]
            self.complete_plugin_maybe_resolve();
            for chunk in std::mem::take(&mut self.editor.lua_queue) {
                if let Err(e) = self.lua.exec(&chunk) {
                    self.editor.echo(format!("E5108: Error executing lua: {e}"));
                }
                self.apply_lua_effects();
            }
            // `:={expr}` / `:lua= {expr}` queued a `vim.print` chunk above (its output
            // has now landed in `:messages` via `apply_lua_effects`); pop the messages
            // panel so the printed value is visible. Ordered after the drain so the
            // panel shows the freshly-recorded line, not the state before it.
            if std::mem::take(&mut self.editor.open_messages_after_lua) {
                self.editor.ex_messages();
            }
            for cmd in std::mem::take(&mut self.editor.deferred_commands) {
                match cmd {
                    DeferredCmd::Server { cmd, range } => self.resolve_command(&cmd, range),
                    // The tail of a `|` chain, held back until the segment ahead of it
                    // resolved (just above). Vim abandons the rest of a command line
                    // once a command errors, so a failed segment drops its tail.
                    DeferredCmd::Chain(c) => {
                        if !self.editor.message_error {
                            self.editor.command(&c);
                        }
                    }
                }
            }
            // `<CR>` selections on a focused `btv.view` buffer: fire the view's Lua
            // `on_select(line, userdata)` handler. The callback may itself queue lua /
            // view ops (a file tree expanding a node, opening a file), so this is
            // inside the fixpoint, draining effects after each.
            for (id, line) in std::mem::take(&mut self.editor.view_selects) {
                if let Err(e) = self.lua.run_view_select(id, line) {
                    self.editor
                        .echo(format!("E5108: Error in btv.view on_select: {e}"));
                }
                self.apply_lua_effects();
            }
            // A user-closed `btv.view` window (`:q`/`:close` on a view buffer): fire the
            // view's Lua `on_close()` so the owning plugin can tear down a group of
            // related views (e.g. bemtvi-diff closing all panes when one is `:q`'d). Like
            // the selects above, the handler may queue lua / view ops, so it drains
            // inside the fixpoint.
            for id in std::mem::take(&mut self.editor.view_closes) {
                if let Err(e) = self.lua.run_view_closed(id) {
                    self.editor
                        .echo(format!("E5108: Error in btv.view on_close: {e}"));
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
            // Picker prompt edits needing a source re-run — a query edit on a
            // **dynamic** source, or an include/exclude edit on any filterable one.
            // Drained *before* `menu_results` and *before* the candidate pushes (which
            // `apply_lua_effects` already gated on the live generation) — the generation
            // was bumped synchronously in core on the keystroke, so a late push from the
            // superseded run is already dropped. Running the source reaps the prior job
            // (`on_cancel`) Lua-side.
            for run in std::mem::take(&mut self.editor.picker_query_changes) {
                if let Err(e) =
                    self.lua
                        .run_picker_run(run.gen, &run.query, &run.include, &run.exclude)
                {
                    self.editor
                        .echo(format!("E5108: Error in btv.picker source: {e}"));
                }
                self.apply_lua_effects();
            }
            // Completion triggers with an **async** source: dispatch the configured
            // sources for the new prefix off the input path (debounced + reaped
            // Lua-side). The generation was bumped synchronously in core on the
            // keystroke, so a late push from a superseded prefix is already gated out
            // when `apply_lua_effects` feeds it. The buffer-source rows are already
            // seeded; these sources only append. Phase 4-B.
            for (gen, ctx) in std::mem::take(&mut self.editor.complete_query_changes) {
                // A fresh run rebuilds the menu, so the previous run's plugin resolve
                // handles are dead (Lua drops them too) — clear the docs cache so a
                // reused handle id can't surface stale docs (Phase 4-E).
                #[cfg(feature = "native")]
                {
                    self.complete_resolve_docs.clear();
                    self.complete_resolve_inflight = None;
                }
                if let Err(e) = self.lua.run_complete_run(
                    gen,
                    &ctx.prefix,
                    ctx.buf,
                    ctx.row,
                    ctx.col,
                    ctx.manual,
                ) {
                    self.editor
                        .echo(format!("E5108: Error in btv.complete source: {e}"));
                }
                // Each native source honors its own `min_chars` too: contribute only
                // once the prefix reaches its threshold (a manual trigger bypasses).
                let plen = ctx.prefix.chars().count();
                // The built-in `lsp` source is server-native: issue (or re-serve a
                // cached) `textDocument/completion` for this trigger; the reply
                // streams into the menu (gen-gated) via `on_completion_reply`.
                if ctx.manual || plen >= self.complete_lsp_min_chars {
                    self.complete_lsp_dispatch(gen);
                }
                // The built-in `snippets` source is feature-agnostic (core engine).
                if ctx.manual || plen >= self.complete_snippets_min_chars {
                    self.complete_snippet_dispatch(gen);
                }
                self.apply_lua_effects();
            }
            // Signature-help auto-trigger: a trigger keystroke (`(` / `,`) raised a
            // one-shot request in core; issue `textDocument/signatureHelp` for it (when
            // the current buffer's server supports it). Shared by the native server and
            // the wasm edit-host, so the demo gets it over the LSP seam too.
            self.drain_signature_auto_request();
            // Command-line completion (`btv.cmdline_complete`): core stamped the token
            // being completed on `<Tab>` (or an edit while the wildmenu is open).
            // Resolve it synchronously against the bundled catalog source — the filter
            // is a microsecond table scan, so unlike the insert sources there is no
            // streaming / generation machinery — and rebuild the menu from the result.
            if let Some(req) = self.editor.cmdline_complete_request.take() {
                let docs = self.editor.cmdline_complete_docs();
                match self.lua.run_cmdline_complete(&req.line, req.col) {
                    Ok(bemtvi_lua::CmdlineComplete::Candidates(cands)) => {
                        // The ex catalog never sets an explicit replace span (`None`):
                        // it completes the trailing token via the menu anchor.
                        let cands: Vec<bemtvi_core::CmdlineCandidate> = cands
                            .into_iter()
                            .map(|(label, insert, doc)| {
                                (label, insert, (!doc.is_empty()).then_some(doc), None)
                            })
                            .collect();
                        self.editor.open_cmdline_menu(
                            req.anchor,
                            req.anchor_width,
                            &req.prefix,
                            cands,
                            docs,
                        );
                    }
                    // A file argument (`:e <Tab>`, …): the source launched the file
                    // picker (queued in this pass's `take_picker_opens`, drained by
                    // `apply_lua_effects` below). The command line stays OPEN underneath
                    // — the picker grabs input while it lives (a `Picker` key context
                    // wins over Command mode), and on confirm the source pastes the
                    // chosen path into the argument token via `btv._cmdline_set_arg`,
                    // leaving the line for the user to run with `<CR>` (no auto-execute).
                    Ok(bemtvi_lua::CmdlineComplete::PickerLaunched) => {}
                    Err(e) => self
                        .editor
                        .echo(format!("E5108: Error in btv.cmdline_complete source: {e}")),
                }
                self.apply_lua_effects();
            }
            // Prompt completion (`btv.ui.input{ complete = fn }`): core stamped the
            // token on `<Tab>` (or an edit while the wildmenu is open). Unlike the ex
            // catalog, the source is the prompt's own `complete` callback and may be
            // async (the DAP `completions` request is a round-trip), so this only
            // *drives* it here — the candidates arrive via `btv._prompt_complete_show`
            // (drained above into `open_prompt_complete_menu`), sync or a tick later.
            if let Some(req) = self.editor.prompt_complete_request.take() {
                // `req.refresh` distinguishes an edit narrowing the open menu from the
                // initial `<Tab>`: the Lua side queries the initial one at once but
                // debounces refreshes (`complete_debounce`).
                if let Err(e) = self
                    .lua
                    .run_prompt_complete(&req.line, req.col, req.refresh)
                {
                    self.editor.echo(format!(
                        "E5108: Error in btv.ui.input completion source: {e}"
                    ));
                }
                self.apply_lua_effects();
            }
            // btv.decor: a window whose visible range changed (scroll / resize / edit
            // reflow) — core stamped a viewport generation per window; dispatch the
            // registered providers off the frame here. Drain the signal unconditionally
            // so it can't accumulate, but only build the snapshot + re-enter Lua when a
            // provider is registered (the common no-provider config pays nothing). A
            // publish the provider produces carries its `generation`, gated at apply
            // time (Phase 3); the generation already moved on for any window scrolled
            // again since, so a stale publish is dropped.
            if self.lua.has_decor_providers() {
                // Recompute the viewport-changed signal here, not only at the
                // `Editor::input` tail: a viewport change driven *off* the input tick —
                // a `:e` that ran via a queued command-line action, a buffer switch from
                // a Lua callback, a relayout — wouldn't otherwise re-run the input-tail
                // detector, so its dispatch would wait for the next keystroke (the file
                // would open uncoloured until you pressed a key). This chokepoint makes
                // the dirty list reflect the current viewport at drain time, whatever
                // moved it. Idempotent: a stable viewport queues nothing.
                //
                // NOTE: this is the single re-detection point — a *new* queued action
                // (widget-keys actions, scheduled callbacks, any future off-input-tick
                // buffer/viewport mutation) is already covered here and must NOT call
                // `recompute_decor_dirty` itself.
                self.editor.recompute_decor_dirty();
                for vp in self.editor.take_decor_dirty() {
                    self.dispatch_decor(vp);
                    self.apply_lua_effects();
                }
            } else {
                // No provider: drain the signal so it can't accumulate, but never build
                // the snapshot or re-enter Lua (the common config pays nothing).
                self.editor.take_decor_dirty();
            }
            // btv.on_key_pending: the matcher's withheld prefix settled this batch —
            // push the pending-key signal (which-key / showcmd) iff it *changed* since
            // the last push. Inside the fixpoint so a handler that opens a float has
            // its effect applied + drained here (and the change-gate makes a repeat
            // round a no-op). Gated on a registered listener, so the common config
            // never reaches the trie walk.
            self.emit_key_pending();
            // Float-list widget results: a confirmed (`Some(key)`) or cancelled
            // (`None`) outcome fires the waiting consumer off the same tick, inside
            // the fixpoint (it may open another widget / queue lua). A `btv.picker`
            // routes to its source (`run_picker_result`, which closes the active
            // picker); a `btv.ui.select` routes to its pending callback. One widget
            // is open at a time, so the two are mutually exclusive.
            for result in std::mem::take(&mut self.editor.menu_results) {
                // The LSP code-action chooser is a native select menu: confirming a
                // row applies that action (neovim's `vim.ui.select` model), cancel is
                // a no-op. Checked first so it can't be misrouted to a Lua callback.
                #[cfg(feature = "native")]
                if std::mem::take(&mut self.pending_code_action) {
                    if let Some(idx) = result {
                        self.apply_code_action(idx);
                    } else {
                        // Cancelled the chooser (Esc): settle the async `code_action`
                        // promise `nil` (no effect) rather than leave it hanging.
                        let cb = std::mem::take(&mut self.code_action_cb);
                        self.settle_lsp_promise(cb, bemtvi_lsp::serde_json::Value::Null);
                    }
                    self.apply_lua_effects();
                    continue;
                }
                if let Some(id) = self.pending_ui_select.take() {
                    if let Err(e) = self.lua.run_ui_select(id, result) {
                        self.editor
                            .echo(format!("E5108: Error in btv.ui.select callback: {e}"));
                    }
                    self.apply_lua_effects();
                } else if self.picker_active {
                    self.picker_active = false;
                    // The confirm gesture's open mode (`<C-t>`/`<C-x>`/`<C-v>` ⇒ tab /
                    // split / vsplit). Taken per-result so it never leaks to the next.
                    let mode = std::mem::take(&mut self.editor.picker_confirm_mode).as_str();
                    // The resume snapshot's window keys — Lua keeps just those item
                    // tables for `confirm` after a `btv.picker.resume()`.
                    let resume_keys = std::mem::take(&mut self.editor.picker_resume_keys);
                    if let Err(e) = self.lua.run_picker_result(result, mode, &resume_keys) {
                        self.editor
                            .echo(format!("E5108: Error in btv.picker confirm: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // A filterable picker just closed: fold the lines its boxes held into the
            // persisted history, so they can be recalled next time (and next session).
            // Driven from the core's capture rather than from the last source run —
            // a dynamic source's re-run is debounced, so the run may lag the final
            // keystroke by a pattern or two.
            if let Some((include, exclude)) = self.editor.picker_closed_filters.take() {
                if let Err(e) = self.lua.run_picker_history_record(&include, &exclude) {
                    self.editor
                        .echo(format!("E5108: Error in btv.picker filter history: {e}"));
                }
                self.apply_lua_effects();
            }
            // "Send the picker's current results to a list" (the `send_to_list`
            // picker action): the action already closed the picker, so deliver the
            // matched keys (and the live query) to Lua, which builds the named list.
            for (keys, query) in std::mem::take(&mut self.editor.picker_sends) {
                self.picker_active = false;
                let resume_keys = std::mem::take(&mut self.editor.picker_resume_keys);
                if let Err(e) = self.lua.run_picker_send(keys, &query, &resume_keys) {
                    self.editor
                        .echo(format!("E5108: Error in btv.picker send: {e}"));
                }
                self.apply_lua_effects();
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
                && self.editor.view_selects.is_empty()
                && self.editor.view_closes.is_empty()
                && self.editor.prompt_results.is_empty()
                && self.editor.menu_results.is_empty()
                && self.editor.picker_sends.is_empty()
                && self.editor.picker_query_changes.is_empty()
                && self.editor.complete_query_changes.is_empty()
                && self.scheduled.is_empty()
                && !self.editor.has_pending_checktime()
                && !self.editor.has_pending_pre_writes()
                && self.au_gate_done.is_empty()
                && !self.editor.has_write_events()
                // A quit committed *inside* this fixpoint (a deferred `:qa`, a `:wqa` replay)
                // needs another round for `drive_exit` to consume the intent and begin the
                // gated sequence; a *parked* sequence (waiting on an async handler) has already
                // consumed it, so the loop breaks and resumes on the settle.
                && !self.editor.has_exit_requested()
                // A `btv.decor.invalidate` raised *after* this round's decor dispatch —
                // from inside a provider, or from a promise continuation that resolved
                // mid-round — owes another round, or the re-dispatch would sit until the
                // next keystroke (the buffer would stay drawn with the stale data). The
                // dispatch step's `take_decor_dirty` consumes the flag, so this can only
                // hold the loop for one extra round per invalidation.
                && !self.editor.decor_invalidation_pending()
            {
                break;
            }
            rounds += 1;
            if rounds >= MAX_ROUNDS {
                // Drop the still-growing work and report it, rather than loop
                // forever. The editor stays responsive to the next message.
                self.editor.lua_queue.clear();
                self.editor.deferred_commands.clear();
                self.editor.view_selects.clear();
                self.editor.view_closes.clear();
                self.editor.prompt_results.clear();
                self.editor.menu_results.clear();
                self.editor.picker_sends.clear();
                self.editor.picker_query_changes.clear();
                self.editor.complete_query_changes.clear();
                self.editor.take_pending_checktime();
                self.editor.take_pending_pre_writes();
                self.au_gate_done.clear();
                // Abandon any in-flight exit sequence too: a runaway handler shouldn't wedge
                // the editor half-quit. It stays alive/responsive (E132 below); the user can
                // re-issue `:qa!`.
                self.exit_stage = None;
                self.exit_gate = None;
                self.editor.take_exit_requested();
                self.editor.take_write_events();
                self.scheduled.clear();
                self.editor
                    .echo("E132: command recursion limit exceeded".to_string());
                break;
            }
        }
        // The fixpoint settled: close the decor pass, so each window's once-per-pass
        // `btv.decor.invalidate` slot is fresh for the next convergence. An ask that
        // was paced (raised in response to this pass's own re-dispatch) is still
        // outstanding and is served first thing next pass.
        self.editor.settle_decor_pass();
        // The drained work may have changed the buffer/window topology (a queued
        // `:lua` window op, a `vim.cmd('split')`, a buffer switch). Diff once more
        // so the resulting `WinNew`/`WinEnter`/`BufEnter`/… autocmds fire — the
        // batch boundary, after everything has settled. Idempotent: a no-op when
        // nothing changed since the last per-key diff (the common case).
        self.emit_lifecycle_events();
        // Refresh the completion docs float beside the popup (the doc-float-window
        // replacement for the old `menu.docs` overlay) now that the selection / any
        // landed resolve is final. It's excluded from the lifecycle diff above
        // (`is_doc_float_window`), so replacing / repositioning it each keystroke fires
        // no user window autocmds.
        self.sync_complete_docs_float();
        self.sync_cmdline_docs_float();
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
        self.drain_pending_opens();
        // Explicit `:wshada` / `:rshada` raised this convergence: flush / re-merge the
        // store. After the opens/saves drain so a `:rshada` sees the settled session.
        // Native only — the shada store (redb) is gated off the wasm build (slice 5a).
        #[cfg(feature = "native")]
        self.drain_pending_shada();
        // Terminal ops core queued this convergence (`:terminal` opens, keystrokes
        // forwarded in terminal mode, kills): spawn / write / kill the PTY. The
        // child's output returns inbound on the `term_events` arm.
        let term_ops = self.editor.take_pending_terminal();
        if !term_ops.is_empty() {
            self.dispatch_terminal_ops(term_ops);
        }
        // The `btv.statusline` segment registry: re-render any invalidated custom
        // segments — and, when the window layout changed, all of them — per window,
        // now that the topology has settled. Last, so it sees the final windows.
        self.refresh_statusline_segments();
        // An `'httphost'`/`'httpport'` write this convergence (a `:set`, which never
        // touches Lua, or an `btv.o` assignment) moves the mount listener. Cheap and
        // gated: returns on the first line unless a plugin has actually mounted.
        #[cfg(feature = "native")]
        self.sync_http_listen_addr();
        // A `:wqa` / `:xa` whose batch just completed (its gate emptied this convergence,
        // via a synchronous commit above or an off-tick ack a prior tick): replay `:qa`
        // now that the editor is clean across the batch. Deferred to here — not run where
        // the gate was advanced — because that may be mid-fixpoint. `run_command` re-enters
        // `run_pending`, whose tail then finds no replay pending (one-shot `take`).
        if let Some(bang) = self.quit_all_replay.take() {
            self.run_command(if bang { "qa!" } else { "qa" });
        }
    }

    /// Re-render the custom `btv.statusline` segments whose cache is stale and fold
    /// the results into the per-`(window, name)` cache the redraw path reads. The
    /// settle point for the segment registry: runs once per [`Self::run_pending`],
    /// after the window/buffer topology has converged, so each segment renders
    /// against the final per-window `{ buf, win, focused }` contexts.
    ///
    /// A segment is re-rendered when it was invalidated (`btv.statusline.invalidate`
    /// or a declared autocmd event) or when the window layout changed — a split /
    /// close, a focus move, or a window swapping its buffer — which would otherwise
    /// leave a window with no (or a stale `focused` / `buf`) cell. Cache entries for
    /// closed windows are pruned. A cheap no-op when no layout is active or nothing
    /// changed (the common path): only an id/buffer-vector compare, no Lua.
    fn refresh_statusline_segments(&mut self) {
        // No segment layout active anywhere (global or window-local): discard any
        // stray invalidations and stop (the `%`-format path owns the status line).
        if !self.statusline_active() {
            self.lua.take_statusline_invalidates();
            self.statusline_pending.clear();
            return;
        }
        // Fold in invalidations queued since the last drain (including any the final
        // `emit_lifecycle_events` above fired into the autocmd dispatch).
        self.fold_statusline_invalidates();

        // The current window layout: `(window id, buffer id)` per window plus the
        // focused window. A change re-renders every custom segment (so per-window
        // `focused` / `buf` stays correct), prunes the cache and window-local
        // overrides to live windows, and rebuilds the custom set (a closed window's
        // local layout may have been the only reference to a segment).
        let wins = self.editor.window_ids();
        let key: Vec<(u64, u64)> = wins
            .iter()
            .map(|&w| (w.0, self.editor.window_buffer(w).map(|b| b.0).unwrap_or(0)))
            .collect();
        let focus = self.editor.current_window_id().0;
        if self.statusline_layout_key.as_ref() != Some(&(key.clone(), focus)) {
            let live: std::collections::HashSet<u64> = wins.iter().map(|w| w.0).collect();
            self.statusline_window.retain(|w, _| live.contains(&w.0));
            self.statusline_cache.retain(|(w, _), _| live.contains(w));
            self.recompute_statusline_custom();
            self.statusline_layout_key = Some((key, focus));
        }

        if self.statusline_pending.is_empty() {
            return;
        }
        // Refresh the window mirror so the Lua re-render reads the settled layout
        // (`btv.win.list()` / `btv.win.buf()` / `btv.win.current()`), then render each
        // dirty segment for every window. The publishes land in `statusline_publishes`.
        self.push_buf_mirror();
        for name in std::mem::take(&mut self.statusline_pending) {
            if let Err(e) = self.lua.run_statusline_rerender(&name) {
                self.editor
                    .echo(format!("E5108: Error rendering statusline segment: {e}"));
            }
        }
        self.fold_statusline_publishes();

        // A segment's `render` may DEFINE highlight groups and reference them in the
        // cells it just published — a powerline statusline (bemtvi-line) lazily creates
        // its separator/transition groups this way. `take_highlights` ran at the top of
        // `run_pending`, before these renders, so those defines are still queued; fold
        // them now so the redraw that projects these cells resolves their colours on the
        // first frame. Without this they resolve to the base look for one tick — an
        // uncoloured-separator flicker until the next tick folds them.
        for hl in self.lua.take_highlights() {
            self.editor.highlights.set_ns(hl.ns, &hl.name, hl_def(&hl));
        }
    }

    /// Fold queued custom-segment invalidations (`btv.statusline.invalidate`, and
    /// the autocmd callbacks a declared `events` list installs) into the pending
    /// set; each dirty segment re-renders per window once the input settles.
    fn fold_statusline_invalidates(&mut self) {
        for name in self.lua.take_statusline_invalidates() {
            if self.statusline_custom.contains(&name) {
                self.statusline_pending.insert(name);
            }
        }
    }

    /// Fold queued custom-segment cell publishes (`btv._statusline_publish`) into
    /// the per-`(win, name)` cache the redraw path reads. Produced only while
    /// `refresh_statusline_segments` re-renders, so this is empty on the common
    /// path.
    fn fold_statusline_publishes(&mut self) {
        for req in self.lua.take_statusline_publishes() {
            let cells = req
                .cells
                .into_iter()
                .map(
                    |(text, group, on_click)| bemtvi_core::statusline::StatusSegment {
                        text,
                        group,
                        on_click,
                    },
                )
                .collect();
            self.statusline_cache.insert((req.win, req.name), cells);
        }
    }

    /// Whether any `btv.statusline` segment layout is active — the global layout or
    /// at least one window-local [`Segments`](WindowStatusline::Segments) override.
    /// A window-only `Format` override doesn't count (it shows the `%`-format).
    fn statusline_active(&self) -> bool {
        self.statusline_layout.is_some()
            || self
                .statusline_window
                .values()
                .any(|w| matches!(w, WindowStatusline::Segments(_)))
    }

    /// Rebuild [`statusline_custom`](EditHost::statusline_custom) — the custom
    /// (non-built-in) segment names referenced by the global layout and every
    /// window-local one — and mark them all pending so the next settle re-renders
    /// them. Called whenever the active layouts change.
    fn recompute_statusline_custom(&mut self) {
        let mut custom: Vec<String> = Vec::new();
        let mut collect = |layout: &bemtvi_core::statusline::SegmentLayout| {
            for name in layout.left.iter().chain(layout.right.iter()) {
                if !bemtvi_core::statusline::is_builtin_segment(name) && !custom.contains(name) {
                    custom.push(name.clone());
                }
            }
        };
        if let Some(layout) = &self.statusline_layout {
            collect(layout);
        }
        for win in self.statusline_window.values() {
            if let WindowStatusline::Segments(layout) = win {
                collect(layout);
            }
        }
        self.statusline_pending.extend(custom.iter().cloned());
        self.statusline_custom = custom;
    }
}

/// Translate a Lua-side `nvim_set_hl` definition into the core registry's
/// `HlDef`, parsing the color strings (`#rrggbb` / named / `NONE`) here at the
/// boundary so `bemtvi-lua` need not know about the color type.
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

/// Parse a size spec into an [`Extent`](bemtvi_core::Extent), or `None` for an
/// empty / unparseable spec (the caller chooses what `None` means — the picker
/// default, or a loud error for a float that requires a size). The single size
/// parser shared by every surface (pickers, floats, `btv.view`, the panel). A bare
/// integer is a cell count (`"100"`); a `vw` / `vh` / `%` suffix is a CSS-style
/// viewport fraction (`"80vw"` → 80% of the reference dimension), clamped to a sane
/// range so a fat-fingered `"500%"` can't paint off-screen.
pub(crate) fn parse_extent(spec: &str) -> Option<bemtvi_core::Extent> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let frac = spec
        .strip_suffix("vw")
        .or_else(|| spec.strip_suffix("vh"))
        .or_else(|| spec.strip_suffix('%'));
    if let Some(num) = frac {
        return num
            .trim()
            .parse::<f32>()
            .ok()
            // `f32::from_str` accepts `nan`/`inf`, and `NaN.clamp(…)` stays NaN —
            // which would silently size a zero float instead of the loud
            // invalid-spec error every other bad value gets. Reject non-finite.
            .filter(|n| n.is_finite())
            .map(|n| bemtvi_core::Extent::Frac((n / 100.0).clamp(0.1, 1.0)));
    }
    spec.parse::<u16>().ok().map(bemtvi_core::Extent::Cells)
}
