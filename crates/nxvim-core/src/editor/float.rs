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
use crate::view::ContentFloatView;

/// An open content float: the lines to render, an optional title drawn on the top
/// border, the border style, and whether it anchors at the cursor or centers over
/// the editor. No selection or query state — it is display-only.
#[derive(Clone, Debug)]
pub(crate) struct ContentFloat {
    pub lines: Vec<String>,
    pub title: Option<String>,
    pub border: BorderStyle,
    pub placement: MenuPlacement,
}

impl Editor {
    /// Open a content float rendering `lines` (hover markup, a signature, or an
    /// `nx.ui.float` caller's content). An empty `lines` opens nothing (and clears
    /// any open float) — there is no empty popup. Replaces any float already open.
    /// Non-grabbing: the next key dismisses it.
    pub fn open_content_float(
        &mut self,
        lines: Vec<String>,
        title: Option<String>,
        border: BorderStyle,
        placement: MenuPlacement,
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
        });
    }

    /// Close the content float if one is open; returns whether anything closed (so
    /// a caller can decide whether a repaint is owed).
    pub fn close_content_float(&mut self) -> bool {
        self.content_float.take().is_some()
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
