//! Two list-less floating surfaces, both transient (dismissed by the next key in
//! [`Editor::input`], the way vim closes a hover float on the next motion) and both
//! never grabbing input:
//!
//! - The **content float** — `nx.ui.float` (and the which-key surface). The sibling
//!   of the floating selectable-list [`Menu`](super::menu) on the shared float
//!   placement layer
//!   ([the float-widget spec](../../../../docs/specs/2026-06-14-nx-ui-float-widget.md),
//!   "What stays out of this widget"): it renders styled content **lines** with no
//!   list and no selection — no filtering, prompt, or confirm. It lives *outside*
//!   the window tree; the server projects its geometry (`project_content_float`),
//!   and core owns only the content and where it floats (cursor vs editor).
//!
//! - The **doc float** ([`Editor::open_doc_float`]) — the LSP hover / signature-help
//!   surface. A *real*, non-focusable float **window** backed by a reused scratch
//!   buffer, so (unlike the content float) it inherits mouse hit-testing, **wheel
//!   scroll**, and the normal window render path for free — the neovim model, where
//!   a hover is just a float over a scratch buffer. A mouse wheel scrolls it; the
//!   next *key* dismisses it.

use super::menu::{Extent, MenuPlacement};
use super::windows::{BorderStyle, FloatAnchor, FloatConfig, FloatRelative};
use super::{BufferId, Editor, WindowId};

/// The surface name of the signature-help doc float — shared by the host (which opens
/// it in `show_signature_help`) and core's auto-trigger session (which keeps it sticky
/// across keystrokes and closes it when the call ends). One constant so the two agree.
pub(crate) const SIGNATURE_DOC_FLOAT: &str = "[Signature]";

/// The surface name of the **completion docs** doc float — the sidebar beside the
/// completion popup showing the selected item's documentation. Like the signature
/// float it persists across keystrokes while its owning widget (the completion menu)
/// is open; see [`Editor::close_transient_doc_floats`].
pub(crate) const COMPLETION_DOC_FLOAT: &str = "[CompletionDocs]";

/// The surface name of the **cmdline wildmenu docs** doc float — the help sidebar
/// beside the command-line completion popup. Plain text (no markdown render).
pub(crate) const CMDLINE_DOC_FLOAT: &str = "[CmdlineDocs]";
use crate::buffer::Buffer;
use crate::extmark::{VirtChunk, VirtDecor, DEFAULT_PRIORITY, DOC_MD_NS};
use crate::unicode::display_width;
use crate::view::ContentFloatView;

/// Wrap plain text lines (the LSP hover / signature surface, and any caller with
/// no styling to express) into the chunked content-float form: one unstyled
/// [`VirtChunk`] per line. A styled caller (`nx.ui.float` with chunk lines —
/// which-key) builds its own runs instead.
pub(crate) fn plain_float_lines(lines: Vec<String>) -> Vec<Vec<VirtChunk>> {
    lines
        .into_iter()
        .map(|text| {
            vec![VirtChunk {
                text,
                hl_group: None,
            }]
        })
        .collect()
}

/// The identity of an open completion docs float — its markdown, the popup box it was
/// placed beside (`WindowId` + the box's `menu_geom` col/row/width), and `wrap`. When
/// these are all unchanged [`Editor::open_completion_docs_float`] leaves the float
/// (and its scroll offset) alone rather than closing + re-opening it; a keystroke that
/// moves the popup or changes the selected row shifts the signature and re-places it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompletionDocsSig {
    markdown: String,
    win: WindowId,
    box_col: usize,
    box_row: usize,
    box_width: usize,
    wrap: bool,
}

/// An open content float: the lines to render (each a run of styled
/// [`VirtChunk`]s, like `virt_lines`), an optional title drawn on the top border,
/// the border style, and whether it anchors at the cursor or centers over the
/// editor. No selection or query state — it is display-only.
#[derive(Clone, Debug)]
pub(crate) struct ContentFloat {
    pub lines: Vec<Vec<VirtChunk>>,
    pub title: Option<String>,
    pub border: BorderStyle,
    pub placement: MenuPlacement,
    /// `true` for a **persistent** float (a `persist`-flagged `nx.ui.float`): it
    /// survives keystrokes and is closed only explicitly (`close_content_float_id`)
    /// or when replaced. `false` for the transient default (hover / signature /
    /// diagnostic / a plain `nx.ui.float`): the next key dismisses it.
    pub persistent: bool,
    /// The handle id a persistent float is keyed by, so `:update`/`:close` from
    /// Lua target the right float and a *stale* handle's close no-ops. `0` for a
    /// transient float (which has no handle).
    pub id: u64,
}

impl Editor {
    /// Open a **transient** content float from plain text `lines` (the LSP hover /
    /// signature surface). An empty `lines` opens nothing (and clears any open
    /// float) — there is no empty popup. Replaces any float already open.
    /// Non-grabbing: the next key dismisses it. Styled callers go through
    /// [`Editor::open_styled_float`].
    pub fn open_content_float(
        &mut self,
        lines: Vec<String>,
        title: Option<String>,
        border: BorderStyle,
        placement: MenuPlacement,
    ) {
        self.set_content_float(plain_float_lines(lines), title, border, placement, 0, false);
    }

    /// Open a content float from chunked `lines` (`nx.ui.float`) — each line a run
    /// of styled [`VirtChunk`]s, so a caller (which-key) can colour keys vs.
    /// descriptions and dim unavailable rows. `id == 0` is the transient default
    /// (dismissed by the next key); a non-zero `id` is a **persistent** float (a
    /// `persist`-flagged handle) — it survives keystrokes until
    /// `close_content_float_id(id)` or a replacement, and an `:update` from the same
    /// handle re-enters here with the same `id`. An empty `lines` closes it.
    pub fn open_styled_float(
        &mut self,
        lines: Vec<Vec<VirtChunk>>,
        title: Option<String>,
        border: BorderStyle,
        placement: MenuPlacement,
        id: u64,
    ) {
        self.set_content_float(lines, title, border, placement, id, id != 0);
    }

    fn set_content_float(
        &mut self,
        lines: Vec<Vec<VirtChunk>>,
        title: Option<String>,
        border: BorderStyle,
        placement: MenuPlacement,
        id: u64,
        persistent: bool,
    ) {
        if lines.is_empty() {
            self.content_float = None;
            return;
        }
        self.content_float = Some(ContentFloat {
            lines,
            title,
            border,
            placement,
            persistent,
            id,
        });
    }

    /// Close the content float if one is open; returns whether anything closed (so
    /// a caller can decide whether a repaint is owed).
    pub fn close_content_float(&mut self) -> bool {
        self.content_float.take().is_some()
    }

    /// Dismiss a *transient* content float (`nx.ui.float`, a hover / diagnostic) —
    /// the "next key wipes it" rule. A *persistent* float (which-key, `persist =
    /// true`) is left alone. Idempotent; returns whether a float was cleared.
    ///
    /// Called from the top of [`input`](Editor::input) for keys that flow through it,
    /// AND from the server's per-key dispatch so a key consumed by a **mapping**
    /// (whose Lua/native RHS runs *outside* `input`) dismisses the float too — else a
    /// mapped key waves nothing away and the float hangs until an unmapped key lands.
    pub fn dismiss_transient_content_float(&mut self) -> bool {
        if matches!(&self.content_float, Some(f) if !f.persistent) {
            self.content_float = None;
            return true;
        }
        false
    }

    /// Close the open content float **only if** it is the persistent float keyed by
    /// `id`. A handle whose float was already replaced (a newer persistent float, a
    /// transient hover) no-ops here rather than closing whatever happens to be open.
    /// Returns whether anything closed.
    pub fn close_content_float_id(&mut self, id: u64) -> bool {
        if matches!(&self.content_float, Some(f) if f.persistent && f.id == id) {
            self.content_float = None;
            return true;
        }
        false
    }

    /// The renderable projection of the open content float, or `None`.
    pub(crate) fn content_float_view(&self) -> Option<ContentFloatView> {
        self.content_float.as_ref().map(|f| ContentFloatView {
            lines: f.lines.clone(),
            title: f.title.clone(),
            border: f.border,
            placement: f.placement,
        })
    }

    /// Open a **doc float** for surface `name`: a read-only, non-focusable floating
    /// *window*, backed by a reused scratch buffer, carrying plain text `lines` (the
    /// LSP hover / signature-help surface). Unlike the styled
    /// [content float](Editor::open_content_float) — an overlay outside the window
    /// tree — this is a *real* float window, so it inherits mouse hit-testing,
    /// **wheel scroll**, and the normal window render path for free (the neovim
    /// model: a hover is a float over a scratch buffer). Like the content float it
    /// is **transient**: the next key dismisses it ([`Editor::input`]); a mouse
    /// wheel, which never flows through `input`, scrolls it instead.
    ///
    /// `name` keys both the reused scratch buffer and the open-window slot, so
    /// re-opening the same surface replaces it in place. An empty `lines` opens
    /// nothing (and leaves any existing float for that surface alone — there is no
    /// empty popup; the caller echoes a message instead).
    ///
    /// `filetype` types the scratch buffer (tree-sitter highlighting defaults on, so
    /// the popup is highlighted in that language for free): `markdown` for the hover
    /// (LSP `MarkupContent`), and the *invoking buffer's* filetype for signature help
    /// (its content is a code signature in the source language). `""` leaves it
    /// untyped.
    pub fn open_doc_float(&mut self, name: &str, lines: Vec<String>, filetype: &str) {
        if lines.is_empty() {
            return;
        }
        // Replace any window already open for this surface (a previous hover that
        // hasn't been dismissed by a key yet).
        self.close_doc_float(name);

        // Size the float to its content: width = the widest line, height = the line
        // count, each clamped so a long hover stays a popup (it scrolls past the
        // cap rather than filling the screen). Absolute cells — the float is
        // re-opened from scratch on the next reply, not reflowed.
        let buf = self.doc_float_buffer(name);
        self.load_str_into(buf, Some(name.to_string()), &lines.join("\n"));
        // `load_str_into` edits the rope directly, so flipping `nomodifiable` after
        // is safe — it refuses only a (never-arriving) user edit of the popup.
        self.buffers.get_mut(buf).buffer.options.modifiable = false;
        // Type the scratch buffer so it gets tree-sitter highlighting for free (ts
        // highlighting defaults on; `ts_language_for` resolves the grammar) — the
        // signature-help popup passes the source buffer's filetype (its content is
        // code in that language). The hover no longer comes through here; it renders
        // *stripped* markdown via [`Editor::open_markdown_float`] instead.
        self.set_filetype(buf, filetype);
        self.place_doc_float(name, buf, &lines);
    }

    /// Render `markdown` in the cursor doc float with its markup *rendered* — the
    /// hover / documentation surface. Unlike [`Editor::open_doc_float`] (which drops
    /// raw lines into a `markdown`-typed buffer and shows the literal `**`/`#`/fences),
    /// this parses the markdown once via [`crate::markdown`] into stripped display
    /// lines + `@markup.*` highlight spans, loads the stripped text into the scratch
    /// buffer, and paints the styling as [`DOC_MD_NS`] extmarks: inline spans, fenced
    /// code blocks syntax-highlighted in their own language (via
    /// [`Editor::preview_highlights`], fail-soft when the grammar is absent), and
    /// thematic breaks as `─` [`line_fill`](crate::extmark::VirtDecor::line_fill)s.
    /// The buffer is left untyped so its own filetype tree-sitter pass never repaints
    /// the stripped text. Empty markup shows nothing.
    pub fn open_markdown_float(&mut self, name: &str, markdown: &str) {
        self.close_doc_float(name);
        let buf = self.doc_float_buffer(name);
        let lines = self.render_markdown_into(buf, name, markdown);
        if lines.is_empty() {
            return;
        }
        self.place_doc_float(name, buf, &lines);
    }

    /// Render `markdown` (via [`crate::markdown`]) into the doc-float scratch buffer
    /// `buf`: load the stripped display lines, leave the buffer untyped, and repaint
    /// its [`DOC_MD_NS`] styling from scratch — inline `@markup.*` spans, thematic-break
    /// [`line_fill`](VirtDecor::line_fill)s, and fenced code blocks syntax-highlighted
    /// in their own language (fail-soft when the grammar is absent). Returns the
    /// stripped lines (for sizing); empty when the markup renders to nothing. Shared by
    /// the hover ([`open_markdown_float`](Self::open_markdown_float)) and completion-docs
    /// ([`open_completion_docs_float`](Self::open_completion_docs_float)) surfaces.
    fn render_markdown_into(&mut self, buf: BufferId, name: &str, markdown: &str) -> Vec<String> {
        let rendered = crate::markdown::render(markdown);
        if rendered.lines.is_empty() {
            return Vec::new();
        }

        self.load_str_into(buf, Some(name.to_string()), &rendered.lines.join("\n"));
        self.buffers.get_mut(buf).buffer.options.modifiable = false;
        // No filetype: styling comes entirely from the extmarks below, so the
        // markdown grammar must not re-highlight the already-stripped text.
        self.set_filetype(buf, "");

        // Repaint from scratch: drop any styling left by a previous reply on this
        // reused buffer, then set the new spans / code highlights / fills.
        {
            let b = &mut self.buffers.get_mut(buf).buffer;
            b.extmarks.clear(DOC_MD_NS, None);
            for span in &rendered.spans {
                if span.line >= b.line_count() {
                    continue;
                }
                let base = b.line_start(span.line);
                let start = base + span.start;
                let end = base + span.end;
                b.extmarks.set(
                    DOC_MD_NS,
                    None,
                    start,
                    Some(end),
                    Some(span.group.to_string()),
                    DEFAULT_PRIORITY,
                    None,
                );
            }
            for fill in &rendered.fills {
                if fill.line >= b.line_count() {
                    continue;
                }
                let at = b.line_start(fill.line);
                b.extmarks.set(
                    DOC_MD_NS,
                    None,
                    at,
                    None,
                    None,
                    DEFAULT_PRIORITY,
                    Some(Box::new(VirtDecor {
                        line_fill: Some(VirtChunk {
                            text: fill.ch.to_string(),
                            hl_group: Some(fill.group.to_string()),
                        }),
                        ..VirtDecor::default()
                    })),
                );
            }
        }

        // Fenced code blocks: highlight each block's text in its own language and
        // lower the resulting spans onto the buffer as extmarks. Fail-soft — a block
        // with no `lang` or no installed grammar simply stays plain (the background
        // above still reads it as code).
        for block in &rendered.code {
            let Some(lang) = block.lang.as_deref() else {
                continue;
            };
            let text = rendered.lines
                [block.first_line..(block.first_line + block.len).min(rendered.lines.len())]
                .join("\n");
            let spans = self.preview_highlights(lang, &text, 0, block.len);
            let b = &mut self.buffers.get_mut(buf).buffer;
            for span in spans {
                let line = block.first_line + span.line;
                if line >= b.line_count() {
                    continue;
                }
                let base = b.line_start(line);
                b.extmarks.set(
                    DOC_MD_NS,
                    None,
                    base + span.start_byte,
                    Some(base + span.end_byte),
                    Some(span.group),
                    DEFAULT_PRIORITY,
                    None,
                );
            }
        }

        rendered.lines
    }

    /// Size the doc float to `lines` (widest line × line count, each clamped so a
    /// long popup scrolls rather than filling the screen), open it as a
    /// non-focusable rounded float below the cursor with `wrap` on, and register it
    /// under `name`. Shared by the plain ([`open_doc_float`](Self::open_doc_float)) and
    /// rendered ([`open_markdown_float`](Self::open_markdown_float)) doc surfaces (the
    /// LSP hover / signature-help popups).
    fn place_doc_float(&mut self, name: &str, buf: BufferId, lines: &[String]) {
        const MAX_W: usize = 80;
        const MAX_H: usize = 20;
        let width = lines
            .iter()
            .map(|l| display_width(l))
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_W) as u16;
        // Height counts the **wrapped** display rows (`wrap` is on below): a line wider
        // than the float — a reflowed markdown paragraph is one long line — spans several
        // rows, so sizing to the raw line count would leave the body one row tall with
        // the rest clipped. Clamp so a huge popup scrolls rather than filling the screen.
        let height =
            crate::unicode::wrapped_row_count(lines, width as usize, true).clamp(1, MAX_H) as u16;
        let cfg = FloatConfig {
            relative: FloatRelative::Cursor,
            anchor: FloatAnchor::NW,
            // Drop below the cursor's own line. `place_float` clamps the box fully
            // on-screen, so a hover near the bottom is pulled up and stays visible.
            row: 1,
            col: 0,
            width: Extent::Cells(width),
            height: Extent::Cells(height),
            focusable: false,
            border: BorderStyle::Rounded,
            ..FloatConfig::default()
        };
        // `enter = false`: focus stays in the editing window; the float is a passive
        // popup — scrolled by the wheel, never focused, never joining `<C-w>` nav.
        let win = self.open_float_window(buf, cfg, false);
        // Wrap a line wider than the float within it (so a long hover paragraph — one
        // reflowed line since markdown collapses its soft breaks — reads fully) rather
        // than truncating at the edge; `wrap` also disables horizontal scroll, so the
        // wheel only moves it vertically.
        self.set_window_option_bool(win, "wrap", true);
        self.doc_float_wins.push((name.to_string(), win));
    }

    /// Render `markdown` into the **completion docs** float and place it beside the open
    /// completion popup — a passive, non-focusable float window (the doc-float model)
    /// that persists across keystrokes while the menu is open
    /// (see [`close_transient_doc_floats`](Self::close_transient_doc_floats)). The
    /// server sources the `markdown` (the selected row's LSP / resolved / inline docs)
    /// and passes `wrap` (the configured `docs_wrap`); **core** owns the placement,
    /// computing it beside the popup box via [`complete_docs_geom`](Self::complete_docs_geom)
    /// — the region-relative twin of the old `redraw.rs::project_complete_docs` geometry,
    /// so the float lands exactly where the server-projected menu overlay does. Empty
    /// markup — or no completion popup / no room beside it — closes the float instead of
    /// showing an empty box. Re-opening replaces the previous float in place.
    pub fn open_completion_docs_float(&mut self, markdown: &str, wrap: bool) {
        // The popup box geometry this float is placed against (region cells) — also the
        // stable part of the signature that decides whether a redundant reopen (which
        // would reset the float's scroll) can be skipped. `None` ⇒ no completion popup.
        let Some((box_col, box_row, box_width, win)) = self.complete_docs_box_geom() else {
            self.close_completion_docs_float();
            return;
        };
        let sig = CompletionDocsSig {
            markdown: markdown.to_string(),
            win,
            box_col,
            box_row,
            box_width,
            wrap,
        };
        // Unchanged since last time and the window is still open: leave it (and its
        // scroll offset) alone. A bare mouse wheel over the float lands here — the
        // selection and popup position didn't move — so the float keeps its scroll.
        if self.completion_docs_sig.as_ref() == Some(&sig)
            && self
                .doc_float_wins
                .iter()
                .any(|(n, _)| n == COMPLETION_DOC_FLOAT)
        {
            return;
        }
        // Render into the reused scratch buffer first (so the width the placement needs
        // reflects the stripped display lines), then place — or close if it renders to
        // nothing / there's no room beside the popup.
        let buf = self.doc_float_buffer(COMPLETION_DOC_FLOAT);
        let lines = self.render_markdown_into(buf, COMPLETION_DOC_FLOAT, markdown);
        if lines.is_empty() {
            self.close_completion_docs_float();
            return;
        }
        let Some((row, col, width, height)) = self.complete_docs_geom(&lines, wrap) else {
            self.close_completion_docs_float();
            return;
        };
        self.close_doc_float(COMPLETION_DOC_FLOAT);
        self.place_doc_float_at(COMPLETION_DOC_FLOAT, buf, row, col, width, height, wrap);
        self.completion_docs_sig = Some(sig);
    }

    /// Close the completion docs float (a no-op if not open). Called at every
    /// completion-close site — the float is not part of the menu view, so it must be
    /// torn down explicitly, the way the signature session closes its float.
    pub fn close_completion_docs_float(&mut self) {
        self.completion_docs_sig = None;
        self.close_doc_float(COMPLETION_DOC_FLOAT);
    }

    /// Show the **cmdline wildmenu docs** float — plain help text (no markdown render)
    /// for the highlighted command-line completion — at an absolute editor cell.
    /// Empty ⇒ closed.
    pub fn open_cmdline_docs_float(
        &mut self,
        lines: Vec<String>,
        row: usize,
        col: usize,
        width: u16,
        max_height: usize,
        wrap: bool,
    ) {
        self.close_doc_float(CMDLINE_DOC_FLOAT);
        if lines.is_empty() {
            return;
        }
        let buf = self.doc_float_buffer(CMDLINE_DOC_FLOAT);
        self.load_str_into(buf, Some(CMDLINE_DOC_FLOAT.to_string()), &lines.join("\n"));
        self.buffers.get_mut(buf).buffer.options.modifiable = false;
        self.set_filetype(buf, "");
        let height = lines.len().clamp(1, max_height.max(1)) as u16;
        self.place_doc_float_at(CMDLINE_DOC_FLOAT, buf, row, col, width, height, wrap);
    }

    /// Close the cmdline wildmenu docs float (a no-op if not open).
    pub fn close_cmdline_docs_float(&mut self) {
        self.close_doc_float(CMDLINE_DOC_FLOAT);
    }

    /// Open a doc float at an **absolute** editor cell (`FloatRelative::Editor`), the
    /// positioned twin of [`place_doc_float`](Self::place_doc_float) (which is
    /// cursor-relative). `row`/`col` are windows-area cells; `place_float` still clamps
    /// the box fully on-screen. `wrap` sets the window's `wrap` option so a long line
    /// wraps within the float rather than truncating.
    #[allow(clippy::too_many_arguments)]
    fn place_doc_float_at(
        &mut self,
        name: &str,
        buf: BufferId,
        row: usize,
        col: usize,
        width: u16,
        height: u16,
        wrap: bool,
    ) {
        let cfg = FloatConfig {
            relative: FloatRelative::Editor,
            anchor: FloatAnchor::NW,
            row: row as isize,
            col: col as isize,
            width: Extent::Cells(width.max(1)),
            height: Extent::Cells(height.max(1)),
            focusable: false,
            border: BorderStyle::Rounded,
            ..FloatConfig::default()
        };
        let win = self.open_float_window(buf, cfg, false);
        self.set_window_option_bool(win, "wrap", wrap);
        // The docs floats (completion / cmdline) render as ordinary float windows, so
        // they inherit the standard `NormalFloat`/`FloatBorder` chrome — the same look as
        // the LSP hover float, rather than the old `menu.docs` sidebar's cmp-specific
        // `CmpDocumentation` theming.
        self.doc_float_wins.push((name.to_string(), win));
    }

    /// The reused scratch buffer for doc-float surface `name`, minting (and
    /// registering) it on first use — the doc-float twin of
    /// [`Editor::named_panel_buffer`]. The same `name` always returns the same
    /// buffer, so re-opening replaces its content in place rather than leaking a
    /// buffer per hover.
    fn doc_float_buffer(&mut self, name: &str) -> BufferId {
        if let Some(b) = self
            .doc_float_buffers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| *b)
        {
            if self.buffers.map.contains_key(&b) {
                return b;
            }
            // A stale registry entry (its buffer was deleted) — drop it, re-mint.
            self.doc_float_buffers.retain(|(n, _)| n != name);
        }
        let id = self.add_buffer(Buffer::empty());
        self.doc_float_buffers.push((name.to_string(), id));
        id
    }

    /// Close the doc-float window for surface `name` if open; the scratch buffer is
    /// kept for reuse. Returns whether a window actually closed.
    pub fn close_doc_float(&mut self, name: &str) -> bool {
        if let Some(pos) = self.doc_float_wins.iter().position(|(n, _)| n == name) {
            let (_, win) = self.doc_float_wins.remove(pos);
            return self.close_window_by_id(win, false);
        }
        false
    }

    /// Dismiss every open doc float — the next-key transient close (the float-window
    /// analogue of clearing [`content_float`](Editor::content_float)). Scratch
    /// buffers are retained for reuse. Returns whether anything closed.
    pub(crate) fn close_all_doc_floats(&mut self) -> bool {
        if self.doc_float_wins.is_empty() {
            return false;
        }
        let wins: Vec<WindowId> = self.doc_float_wins.drain(..).map(|(_, w)| w).collect();
        for win in wins {
            self.close_window_by_id(win, false);
        }
        true
    }

    /// The next-key doc-float dismissal that runs at the top of [`Editor::input`], but
    /// **keeping** the signature float while a signature session is open — so an
    /// auto-triggered signature popup survives the keystrokes that fill the call
    /// instead of flashing away on the first one. With no session it is exactly
    /// [`close_all_doc_floats`](Self::close_all_doc_floats).
    pub(crate) fn close_transient_doc_floats(&mut self) -> bool {
        // The set of doc floats that survive this keystroke: each is owned by a widget
        // whose lifecycle (not the next-key transient close) tears it down. The
        // signature float lives for its call session; the completion/cmdline docs
        // floats live while their popup is open (they refresh every keystroke as you
        // type to filter, and are closed explicitly when the popup closes).
        let mut keep: Vec<&str> = Vec::new();
        if self.signature_session {
            keep.push(SIGNATURE_DOC_FLOAT);
        }
        if self.completion_active() {
            keep.push(COMPLETION_DOC_FLOAT);
        }
        if self.cmdline_complete_active() {
            keep.push(CMDLINE_DOC_FLOAT);
        }
        if keep.is_empty() {
            return self.close_all_doc_floats();
        }
        let mut closed = false;
        let kept: Vec<(String, WindowId)> = std::mem::take(&mut self.doc_float_wins)
            .into_iter()
            .filter_map(|(name, win)| {
                if keep.contains(&name.as_str()) {
                    Some((name, win))
                } else {
                    self.close_window_by_id(win, false);
                    closed = true;
                    None
                }
            })
            .collect();
        self.doc_float_wins = kept;
        closed
    }

    /// Whether `id` is a reused doc-float scratch buffer. Such buffers are surfaces,
    /// not documents: excluded from `:ls` / buffer navigation the way panel buffers
    /// are (see [`Editor::is_panel_buffer`]).
    pub(crate) fn is_doc_float_buffer(&self, id: BufferId) -> bool {
        self.doc_float_buffers.iter().any(|(_, b)| *b == id)
    }

    /// Whether `id` is an open doc-float **window** (hover / signature / completion /
    /// cmdline docs). These are internal UI surfaces, not user windows — the lifecycle
    /// diff excludes them so opening / replacing / repositioning one (the completion
    /// docs float refreshes every keystroke) never fires user `WinNew`/`WinClosed`/
    /// `WinResized`/`WinScrolled` autocmds, the window twin of [`is_doc_float_buffer`].
    pub fn is_doc_float_window(&self, id: WindowId) -> bool {
        self.doc_float_wins.iter().any(|(_, w)| *w == id)
    }
}
