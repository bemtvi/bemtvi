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
use crate::buffer::Buffer;
use crate::extmark::VirtChunk;
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
    /// LSP hover / signature-help surface). Unlike the borderless, styled
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
    pub fn open_doc_float(&mut self, name: &str, lines: Vec<String>) {
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
        const MAX_W: usize = 80;
        const MAX_H: usize = 20;
        let width = lines
            .iter()
            .map(|l| display_width(l))
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_W) as u16;
        let height = lines.len().clamp(1, MAX_H) as u16;

        let buf = self.doc_float_buffer(name);
        self.load_str_into(buf, Some(name.to_string()), &lines.join("\n"));
        // `load_str_into` edits the rope directly, so flipping `nomodifiable` after
        // is safe — it refuses only a (never-arriving) user edit of the popup.
        self.buffers.get_mut(buf).buffer.options.modifiable = false;

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

    /// Whether `id` is a reused doc-float scratch buffer. Such buffers are surfaces,
    /// not documents: excluded from `:ls` / buffer navigation the way panel buffers
    /// are (see [`Editor::is_panel_buffer`]).
    pub(crate) fn is_doc_float_buffer(&self, id: BufferId) -> bool {
        self.doc_float_buffers.iter().any(|(_, b)| *b == id)
    }
}
