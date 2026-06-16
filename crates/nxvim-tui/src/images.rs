//! In-terminal image rendering for `'imagepreview'` (Phase 2): decode an image
//! file once, cache its resizable protocol, and paint it with `ratatui-image`
//! over a preview window's body. Only the live client owns an [`ImageStore`] (the
//! protocol detection queries a real terminal); the headless render / test paths
//! pass `None`, so the image area stays blank there.

use std::collections::HashMap;

use image::{DynamicImage, ImageReader};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

/// The client's image renderer: the terminal-graphics [`Picker`] (protocol + cell
/// pixel size, detected once at startup) plus a path-keyed cache of decoded,
/// resizable image protocols. A `None` cache entry is a decode failure, kept so a
/// broken file isn't re-read every frame.
pub(crate) struct ImageStore {
    picker: Picker,
    cache: HashMap<String, Option<StatefulProtocol>>,
}

impl ImageStore {
    /// Detect the terminal's graphics protocol and cell pixel size by querying it
    /// over stdio. Must run after entering the alternate screen and before the
    /// input reader starts. Detection failure (e.g. no tty / a terminal that
    /// answers nothing) falls back to unicode halfblocks, so previews still render
    /// — just coarser.
    pub(crate) fn new() -> Self {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        ImageStore {
            picker,
            cache: HashMap::new(),
        }
    }

    /// Paint the image at `path` into `area`, decoding (and caching) it on first
    /// use. A file that can't be read/decoded paints a visible placeholder rather
    /// than failing silently. (Cache keyed on path only — re-decoding a file that
    /// changed on disk is a later phase.)
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, path: &str) {
        if !self.cache.contains_key(path) {
            let proto = decode(path).map(|img| self.picker.new_resize_protocol(img));
            self.cache.insert(path.to_string(), proto);
        }
        match self.cache.get_mut(path).and_then(Option::as_mut) {
            // `StatefulImage` re-encodes to fit `area` only when the area changed,
            // so the per-frame cost is just emitting the cached encoding.
            Some(proto) => frame.render_stateful_widget(StatefulImage::new(), area, proto),
            None => {
                frame.render_widget(Paragraph::new(format!("[image: cannot read {path}]")), area)
            }
        }
    }
}

/// Read and decode an image file into a [`DynamicImage`], guessing the format from
/// its contents (not just the extension). `None` on any read / decode error.
fn decode(path: &str) -> Option<DynamicImage> {
    ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}
