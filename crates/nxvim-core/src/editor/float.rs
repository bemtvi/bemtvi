//! The list-less **content float** — `nx.ui.float` and the LSP hover / signature
//! help surface. The sibling of the floating selectable-list [`Menu`](super::menu)
//! on the shared float placement layer
//! ([the float-widget spec](../../../../docs/specs/2026-06-14-nx-ui-float-widget.md),
//! "What stays out of this widget"): it renders plain content **lines** with no
//! list and no selection, so it is much simpler than the menu — no filtering, no
//! prompt, no confirm. It **never grabs input**: a content float is a transient
//! popup dismissed by the next key ([`Editor::input`]), the way vim closes a hover
//! float on the next motion. The server projects its geometry
//! (`project_content_float`); core only owns the content and where it wants to
//! float (cursor vs editor).

use super::menu::MenuPlacement;
use super::windows::BorderStyle;
use super::Editor;
use crate::extmark::VirtChunk;
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
}
