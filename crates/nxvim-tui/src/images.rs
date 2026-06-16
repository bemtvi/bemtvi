//! In-terminal image rendering for `'imagepreview'` (Phase 2): decode an image
//! file once, cache its resizable protocol, and paint it with `ratatui-image`
//! over a preview window's body. Only the live client owns an [`ImageStore`] (the
//! protocol detection queries a real terminal); the headless render / test paths
//! pass `None`, so the image area stays blank there.

use std::collections::HashMap;
use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ImageReader};
use nxvim_view::ImageData;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use tokio::sync::mpsc::UnboundedSender;

/// A request to fetch a remote (daemon-session) preview's bytes over the editor RPC.
/// The store can't read the marker's `path` off its own disk — the file is on the
/// daemon — so it emits one of these; the event loop fulfils it with
/// `nxvim_image_read` and feeds the bytes back via [`ImageStore::deliver`].
pub(crate) struct ImageFetch {
    pub path: String,
    /// The preview's on-disk version `(size, mtime_ms)`, echoed back so a stale reply
    /// for a superseded version is dropped rather than replacing newer bytes.
    pub version: (u64, u64),
}

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
    /// Out-of-band byte fetches for remote (daemon-session) previews, keyed by path —
    /// the bytes aren't on local disk, so they're requested over the editor RPC.
    remote: HashMap<String, RemoteSlot>,
    /// The sink the event loop drains to issue `nxvim_image_read` requests; a reply
    /// comes back via [`ImageStore::deliver`].
    fetch_tx: UnboundedSender<ImageFetch>,
}

/// A remote preview's out-of-band byte fetch (see [`ImageFetch`]), keyed by path.
struct RemoteSlot {
    /// The on-disk version `(size, mtime_ms)` this fetch was issued for; a changed
    /// version (a watch reload) supersedes it with a fresh fetch.
    version: (u64, u64),
    state: RemoteState,
}

/// Where a remote preview's byte fetch stands.
enum RemoteState {
    /// A request is in flight; paint the loading placeholder until it lands.
    Pending,
    /// The fetched bytes, ready to decode.
    Ready(Vec<u8>),
    /// The fetch failed (a read error / vanished file); paint the error placeholder.
    Failed,
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
    pub(crate) fn new(fetch_tx: UnboundedSender<ImageFetch>) -> Self {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        ImageStore {
            picker,
            cache: HashMap::new(),
            remote: HashMap::new(),
            fetch_tx,
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
        // A remote preview's bytes live on the daemon; make sure a fetch is in flight
        // (or already resolved) for this version before deciding what to decode.
        if image.remote {
            self.ensure_remote_fetch(path, version);
        }
        // (Re)decode when there's no entry or the on-disk version moved (the latter
        // also retries a file whose earlier decode failed but has since been fixed).
        // A remote preview decodes the fetched bytes — and skips the (re)decode until
        // they land, keeping any stale entry so a reload doesn't flash the placeholder;
        // a local preview reads the shared disk.
        if self.cache.get(path).map(|e| e.version) != Some(version) {
            if image.remote {
                if let Some(bytes) = self.remote_ready(path, version).map(<[u8]>::to_vec) {
                    let decoded = decode_bytes(&bytes).map(|img| self.decoded_from(img));
                    self.cache
                        .insert(path.to_string(), CacheEntry { version, decoded });
                }
            } else {
                let decoded = decode(path).map(|img| self.decoded_from(img));
                self.cache
                    .insert(path.to_string(), CacheEntry { version, decoded });
            }
        }
        // One cell's pixel size, to convert the image's pixel size into cells.
        let font = self.picker.font_size();
        // Paint a decoded image if one is cached — including a *stale* one kept across
        // a reload, so the picture doesn't flash a placeholder while new bytes fetch.
        if let Some(d) = self.cache.get_mut(path).and_then(|e| e.decoded.as_mut()) {
            let target = centered_fit(area, d.px, (font.width, font.height));
            // `StatefulImage` re-encodes to fit `target` only when it changes, so the
            // per-frame cost is just emitting the cached encoding.
            frame.render_stateful_widget(StatefulImage::new(), target, &mut d.proto);
            return;
        }
        // No usable image: a remote fetch still in flight reads as "loading"; anything
        // else (a decode failure, an errored fetch) is a hard "cannot read".
        let msg = if self.is_loading(path, version) {
            format!("[image: loading {path}]")
        } else {
            format!("[image: cannot read {path}]")
        };
        frame.render_widget(Paragraph::new(msg), area);
    }

    /// Build a [`Decoded`] (resizable protocol + pixel size) from a decoded image.
    fn decoded_from(&mut self, img: DynamicImage) -> Decoded {
        Decoded {
            px: (img.width(), img.height()),
            proto: self.picker.new_resize_protocol(img),
        }
    }

    /// Make sure a fetch over `nxvim_image_read` is in flight (or already resolved) for
    /// `path` at `version`. Sends exactly one request per (path, version): the slot
    /// dedupes resends while a fetch is pending and across frames, and a changed
    /// version (a watch reload) issues a fresh request. The reply returns via
    /// [`deliver`](Self::deliver).
    fn ensure_remote_fetch(&mut self, path: &str, version: (u64, u64)) {
        if self.remote.get(path).map(|s| s.version) == Some(version) {
            return; // already fetching this version, or its bytes/failure are cached
        }
        let _ = self.fetch_tx.send(ImageFetch {
            path: path.to_string(),
            version,
        });
        self.remote.insert(
            path.to_string(),
            RemoteSlot {
                version,
                state: RemoteState::Pending,
            },
        );
    }

    /// The fetched bytes for a remote preview if they've arrived and still match
    /// `version`, else `None` (in flight, failed, or a superseded version).
    fn remote_ready(&self, path: &str, version: (u64, u64)) -> Option<&[u8]> {
        match self.remote.get(path) {
            Some(RemoteSlot {
                version: v,
                state: RemoteState::Ready(bytes),
            }) if *v == version => Some(bytes),
            _ => None,
        }
    }

    /// Whether a remote fetch for `path` at `version` is still in flight (so the
    /// placeholder reads "loading" rather than "cannot read").
    fn is_loading(&self, path: &str, version: (u64, u64)) -> bool {
        matches!(
            self.remote.get(path),
            Some(RemoteSlot { version: v, state: RemoteState::Pending }) if *v == version
        )
    }

    /// Receive an `nxvim_image_read` reply for a remote preview (routed from the event
    /// loop): cache the bytes, or mark the fetch failed, so the next paint
    /// decodes/falls-back. A reply for a version no longer requested is dropped (a
    /// newer fetch superseded it). The caller repaints afterward.
    pub(crate) fn deliver(
        &mut self,
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    ) {
        if self.remote.get(&path).map(|s| s.version) != Some(version) {
            return; // a newer fetch (or a closed preview) superseded this reply
        }
        let state = match result {
            Ok(bytes) => RemoteState::Ready(bytes),
            Err(_) => RemoteState::Failed,
        };
        self.remote.insert(path, RemoteSlot { version, state });
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
    Some(downscale(img))
}

/// Decode in-memory image `bytes` — a remote preview's bytes fetched over the editor
/// RPC (`nxvim_image_read`), which the client can't read off its own disk. Guesses the
/// format from the contents like [`decode`] and applies the same downscale. `None` on a
/// decode error (a corrupt / unsupported file → the placeholder).
fn decode_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some(downscale(img))
}

/// Downscale an oversized image to the preview cap (aspect-preserving), so the retained
/// copy and every re-encode stay bounded regardless of the source size — the shared
/// tail of [`decode`] / [`decode_bytes`].
fn downscale(img: DynamicImage) -> DynamicImage {
    if img.width().max(img.height()) > MAX_EDGE {
        img.resize(MAX_EDGE, MAX_EDGE, FilterType::Triangle)
    } else {
        img
    }
}
