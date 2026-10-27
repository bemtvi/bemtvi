//! The bemtvi GUI client, with no toolkit: the wgpu renderer, fonts, images, the editor
//! sessions and the IO thread that talks to them. A host (a toolkit adapter: the
//! `bemtvi-gui` binary and the `bemtvi-mitsuami` view, both on mitsuami) shows a window,
//! hands its handle to the [`Renderer`], and feeds input in.
//!
//! A thin RPC client that owns no editor state — the GUI sibling of `bemtvi-tui`.
//! It attaches to the server, sends keystrokes as vim key-notation
//! (`btv_input`), and paints the server's [`View`](bemtvi_view::View) (the same
//! `redraw` model the TUI consumes) onto a GPU surface as a monospace cell grid.
//!
//! **Threading.** The host's toolkit owns its UI thread, so the RPC lives on a
//! separate IO thread ([`spawn_io`]) running a current-thread tokio runtime: it drives
//! [`bemtvi_rpc::connect`], decodes each `redraw` into a `View`, and sends it to the UI
//! thread as a [`UserEvent`] over a channel. Input flows the other way without a
//! runtime — [`bemtvi_rpc::Rpc`] is `Clone + Send` and its `notify` is synchronous, so
//! the UI thread fires `btv_input` / `nvim_ui_*` directly on a cloned handle.
//!
//! **Native file dialogs.** A `:` command line that asks for a file dialog (see
//! [`bemtvi_core::dialog`]) is decided by the editor as it runs the line: it hands the
//! client a `btv_file_dialog` request, the client asks its host for the dialog
//! ([`Effect::OpenDialog`] / [`Effect::SaveDialog`]), and runs the real command with
//! the file picked.

mod client;
mod config;
mod fontchain;
mod images;
mod input;
mod io;
pub mod mouse;
mod options;
pub mod remote;
mod render;
pub mod session;

pub use bemtvi_core::dialog::{
    dialog_action, open_dialog_verb, open_path_command, save_dialog_needed, DialogAction,
};
pub use client::{
    Answer, Client, ClientHandle, ClientOptions, CursorShape, Effect, IoThread, PointRect,
    RequestId,
};
pub use config::{caret_blink, view_fonts, GlyphOverflow, GuiConfig, CARET_BLINK};
pub use input::{
    button_name, encode_input, is_paste, mouse_modifier, InputEvent, KeyCode, Modifiers,
    MouseButton, Point, ScrollDelta,
};
pub use io::{report_session_error, spawn_io, IoParts, Proxy, UserEvent};
pub use options::{SessionOptions, SINGLE_FILE_DISABLED_COMMANDS, SINGLE_FILE_DISABLED_PICKERS};
pub use session::{
    parse_connect_uri, spawn_session, spawn_session_with, spawn_stdio_daemon_session,
    spawn_workspace_session, Config, Launch, Session, Shada,
};

// The image store (render thread) emits an `ImageFetch` when it sees a `remote`
// preview it can't read off local disk; the IO thread fulfils it with
// `bemtvi_image_read` and posts the bytes back as [`UserEvent::ImageBytes`]. The
// request type itself is the shared client model's (`bemtvi_view::images`).
pub use bemtvi_view::images::{DecodeReq, ImageFetch};

// The pure inline-inlay-hint geometry (the shift math) and the segment splice, so
// the Tier-1 `inlay` test can exercise them without a GPU — like the mouse helpers.
// The block-element geometry (`▀`, `█`, …), drawn as rectangles rather than glyphs, so
// the Tier-1 `blocks` test can check it without a GPU.
pub use fontchain::runs_by;
pub use render::{block_element_rects, split_block_elements};
pub use render::{
    inlay_shift, run_columns, seg_bg_spans, segment_columns, splice_inlay, split_at_char,
    virt_inline_shift, with_preedit, Seg, DEFAULT_BG, DEFAULT_ERROR_MSG, DEFAULT_FG,
    DEFAULT_INCSEARCH_BG, DEFAULT_INLAY, DEFAULT_LINE_NR, DEFAULT_SEARCH_BG, DEFAULT_STATUS_BG,
    DEFAULT_VISUAL_BG, DEFAULT_WIN_SEPARATOR, DIAG_ERROR, DIAG_HINT, DIAG_INFO, DIAG_WARN,
    MULTICURSOR_ACCENT,
};
pub use render::{shape_with_fonts, TestGlyph};
// The pure per-row syntax-coloring layer (run splitting + the no-colorscheme group
// fallback), exported so the Tier-1 `syntax` test can exercise it without a GPU.
pub use render::{
    apply_search_fg, clear_bg_under, col_to_screen, expand_tabs, group_fallback, rect_subtract,
    row_segments, text_run_origin,
};
// The wide-glyph mask (replace an off-grid emoji cluster with cell-width spaces) and
// the shrink-to-fit sizing of the cluster that mask reserved room for, so the Tier-1
// `wide` test can exercise both without shaping / a GPU.
pub use render::{
    cluster_scale, is_letterform, italic_runs, mask_segments, offgrid_clusters, overflow_ceiling,
    overflow_cells, Ink,
};
// The pure caret-cell math for the command line and the picker prompt (char-offset
// wire fields → display-width cells), exported for the Tier-1 `caret` test.
pub use render::{cmdline_caret_col, popup_covers, query_caret_col};
// The block cursor's reverse-video pair — the opaque quad's colour and the inverted
// colour its covered glyph is re-drawn in — plus that recolor, exported for the
// Tier-1 `cursor` test.
pub use render::{apply_cursor_fg, block_cursor_colors};
// Where a floating menu box anchors — window-relative for a completion popup, but
// screen-relative for the command-line wildmenu. Exported for the Tier-1 `menu` test.
pub use render::menu_box_origin;
// The point-size → device-pixel font size the cell grid is measured at, exported so
// the Tier-1 `grid` test can shape a row at a fractional display scale and check it
// still lands on the grid.
pub use render::physical_font_size;
// The sRGB→linear color conversions feeding the quad pipeline, exported so the Tier-1
// `color` test can pin the channel order without a GPU.
pub use render::{clear_color, color_to_rgba, srgb_to_color, srgb_to_color_rgba};
pub use render::{Renderer, ScrollFrame};
// Re-exported for the Tier-1 band data-contract tests (`tests/scroll_band.rs`).
pub use bemtvi_view::ScrollAnim;

/// The size of the surface the editor is drawn on: physical pixels, and the display's
/// scale factor (physical pixels per point; `1.0` where the host doesn't say).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceSize {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

impl SurfaceSize {
    /// Whether there's nothing to draw on (a zero side).
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}
