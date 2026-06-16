//! In-terminal image rendering for `'imagepreview'` (Phase 2): decode an image
//! file once, cache its resizable protocol, and paint it with `ratatui-image`
//! over a preview window's body. Only the live client owns an [`ImageStore`] (the
//! protocol detection queries a real terminal); the headless render / test paths
//! pass `None`, so the image area stays blank there.

use std::collections::HashMap;

use image::imageops::FilterType;
use image::{DynamicImage, ImageReader};
use nxvim_view::ImageData;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

/// Cap the longest edge of a decoded image (pixels) before building its protocol.
/// A preview never needs more than the terminal can show, so this bounds the held
/// image and the per-resize re-encode cost for a huge file — the never-freeze
/// guard — while staying generous enough that even a big / hi-dpi terminal looks
/// crisp. (The full-resolution decode still happens once; only the retained copy
/// is shrunk.)
const MAX_EDGE: u32 = 2560;

/// The client's image renderer: the terminal-graphics [`Picker`] (protocol + cell
/// pixel size, detected once at startup) plus a path-keyed cache of decoded,
/// resizable images.
pub(crate) struct ImageStore {
    picker: Picker,
    cache: HashMap<String, CacheEntry>,
}

/// One path's cache slot: the file version it was decoded at (size + mtime-ms) and
/// the decoded image, or `None` for a decode failure. Keeping the version lets a
/// changed-on-disk file re-decode (a stale or once-broken entry is replaced) while
/// an unchanged one — success or failure — is never re-read.
struct CacheEntry {
    version: (u64, u64),
    decoded: Option<Decoded>,
}

/// A decoded image ready to paint: its resizable protocol plus the (post-downscale)
/// pixel size, which — with the picker's cell size — gives the aspect-preserving
/// cell rectangle used to center the picture in its window.
struct Decoded {
    proto: StatefulProtocol,
    px: (u32, u32),
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

    /// Paint `image` **centered** within `area`, decoding (and caching) it on first
    /// use and re-decoding when the file changed on disk (its `size`/`mtime_ms`
    /// version moved — e.g. an external edit the watch reloaded). The picture is fit
    /// into `area` preserving its aspect ratio (never upscaled past its natural size)
    /// and centered, so the leftover margins show the window background. A file that
    /// can't be read/decoded paints a visible placeholder rather than failing
    /// silently.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, image: &ImageData) {
        let path = image.path.as_str();
        let version = (image.size, image.mtime_ms);
        // (Re)decode when there's no entry or the on-disk version moved (the latter
        // also retries a file whose earlier decode failed but has since been fixed).
        if self.cache.get(path).map(|e| e.version) != Some(version) {
            let decoded = decode(path).map(|img| Decoded {
                px: (img.width(), img.height()),
                proto: self.picker.new_resize_protocol(img),
            });
            self.cache
                .insert(path.to_string(), CacheEntry { version, decoded });
        }
        // One cell's pixel size, to convert the image's pixel size into cells.
        let font = self.picker.font_size();
        match self.cache.get_mut(path).and_then(|e| e.decoded.as_mut()) {
            Some(d) => {
                let target = centered_fit(area, d.px, (font.width, font.height));
                // `StatefulImage` re-encodes to fit `target` only when it changes,
                // so the per-frame cost is just emitting the cached encoding.
                frame.render_stateful_widget(StatefulImage::new(), target, &mut d.proto);
            }
            None => {
                frame.render_widget(Paragraph::new(format!("[image: cannot read {path}]")), area)
            }
        }
    }
}

/// The largest aspect-preserving sub-rect of `area` the image fits in — never
/// upscaled past its natural cell size (matching `Resize::Fit`) — centered within
/// `area`. `px` is the image's pixel size, `font` one terminal cell's pixel size.
fn centered_fit(area: Rect, px: (u32, u32), font: (u16, u16)) -> Rect {
    let (iw, ih) = px;
    let (fw, fh) = (u32::from(font.0.max(1)), u32::from(font.1.max(1)));
    if iw == 0 || ih == 0 || area.width == 0 || area.height == 0 {
        return area;
    }
    // The image's natural size in cells (round up so a partial cell still shows).
    let nat_w = iw.div_ceil(fw) as f64;
    let nat_h = ih.div_ceil(fh) as f64;
    // Scale down to fit both dimensions; clamp to 1.0 so we never upscale.
    let scale = (area.width as f64 / nat_w)
        .min(area.height as f64 / nat_h)
        .min(1.0);
    let w = ((nat_w * scale).round() as u16).clamp(1, area.width);
    let h = ((nat_h * scale).round() as u16).clamp(1, area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Read and decode an image file into a [`DynamicImage`], guessing the format from
/// its contents (not just the extension). `None` on any read / decode error.
fn decode(path: &str) -> Option<DynamicImage> {
    let img = ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    // Downscale an oversized image to the preview cap (aspect-preserving), so the
    // retained copy and every re-encode stay bounded regardless of the source size.
    Some(if img.width().max(img.height()) > MAX_EDGE {
        img.resize(MAX_EDGE, MAX_EDGE, FilterType::Triangle)
    } else {
        img
    })
}
