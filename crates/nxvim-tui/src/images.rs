//! In-terminal image rendering for `'imagepreview'` (Phase 2): decode an image
//! file once, cache its resizable protocol, and paint it with `ratatui-image`
//! over a preview window's body. Only the live client owns an [`ImageStore`] (the
//! protocol detection queries a real terminal); the headless render / test paths
//! pass `None`, so the image area stays blank there.

use std::collections::HashMap;

use image::DynamicImage;
pub(crate) use nxvim_view::images::ImageFetch;
use nxvim_view::images::{decode_bytes, decode_file, RemoteImages, MAX_EDGE};
use nxvim_view::ImageData;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use tokio::sync::mpsc::UnboundedSender;

// The fetch request / remote byte cache / bounded decode helpers are the
// toolkit-neutral half shared with the GUI; they live in [`nxvim_view::images`].
// (`ImageFetch` is re-exported so the event loop can keep naming
// `images::ImageFetch`.)

/// The client's image renderer: the terminal-graphics [`Picker`] (protocol + cell
/// pixel size, detected once at startup) plus a path-keyed cache of decoded,
/// resizable images.
pub(crate) struct ImageStore {
    picker: Picker,
    cache: HashMap<String, CacheEntry>,
    /// Out-of-band byte fetches for remote (daemon-session) previews (shared with
    /// the GUI; see [`RemoteImages`]).
    remote: RemoteImages,
    /// The sink the event loop drains to issue `nxvim_image_read` requests; a reply
    /// comes back via [`ImageStore::deliver`].
    fetch_tx: UnboundedSender<ImageFetch>,
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
        ImageStore {
            picker: detect_picker(),
            cache: HashMap::new(),
            remote: RemoteImages::new(),
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
            if let Some(req) = self.remote.ensure_fetch(path, version) {
                let _ = self.fetch_tx.send(req);
            }
        }
        // (Re)decode when there's no entry or the on-disk version moved (the latter
        // also retries a file whose earlier decode failed but has since been fixed).
        // A remote preview decodes the fetched bytes — and skips the (re)decode until
        // they land, keeping any stale entry so a reload doesn't flash the placeholder;
        // a local preview reads the shared disk.
        if self.cache.get(path).map(|e| e.version) != Some(version) {
            if image.remote {
                if let Some(bytes) = self.remote.ready(path, version).map(<[u8]>::to_vec) {
                    let decoded = decode_bytes(&bytes, MAX_EDGE).map(|img| self.decoded_from(img));
                    self.cache
                        .insert(path.to_string(), CacheEntry { version, decoded });
                }
            } else {
                let decoded = decode_file(path, MAX_EDGE).map(|img| self.decoded_from(img));
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
        let msg = if self.remote.is_loading(path, version) {
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
        self.remote.deliver(path, version, result);
    }
}

/// Detect the graphics protocol — but only *query* a terminal that provably answers
/// queries; anything else gets the halfblocks fallback outright.
///
/// `Picker::from_query_stdio` (ratatui-image 11.0.4) spawns a helper thread that
/// blocks in `stdin.read()` until it sees the reply to its `ESC[5n` device-status
/// query — its termination sentinel. On a terminal that answers nothing (a dumb
/// PTY, `TERM=dumb`, some ssh hops / multiplexers) that reply never comes: the
/// caller times out and falls back to halfblocks, but the helper thread stays
/// parked on the blocking read. It then *swallows the first keystrokes the user
/// types* (they land in its `read`), and on its way out — its result channel is
/// closed by then, so the loop errors — it runs its `disable_raw_mode()` cleanup,
/// dropping the terminal back to cooked mode. Every later keystroke is then
/// line-buffered by the PTY and never reaches the input reader: all input is dead.
/// (Caught by the PTY e2e tests, `crates/nxvim/tests/e2e.rs`.)
///
/// The probe sends the same `ESC[5n` and waits on `poll(2)`, which can wait
/// *without consuming stdin* and leaves nothing behind on timeout — so a mute
/// terminal costs one bounded wait and keeps a fully working keyboard.
fn detect_picker() -> Picker {
    if terminal_answers_status_query() {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    } else {
        Picker::halfblocks()
    }
}

/// Probe whether the terminal answers an `ESC[5n` device-status report (the query
/// every terminal is expected to implement, and the one `from_query_stdio` relies
/// on to terminate). Waits via [`stdin_readable_within`] — never a blocking read —
/// then drains the pending reply so the *stale* `ESC[0n` can't terminate the real
/// capability query prematurely (its parser treats any status report as "done").
/// Raw mode is already on here (`ratatui::init` ran), so the reply isn't held back
/// by the line discipline.
#[cfg(unix)]
fn terminal_answers_status_query() -> bool {
    use std::io::{Read, Write};

    let mut out = std::io::stdout();
    if out
        .write_all(b"\x1b[5n")
        .and_then(|()| out.flush())
        .is_err()
    {
        return false;
    }
    // Generous first wait: a slow hop (ssh) can sit on the reply for a while.
    if !crate::termquery::stdin_readable_within(std::time::Duration::from_millis(1000)) {
        return false;
    }
    // Drain until a quiet gap so the capability query starts from clean input.
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    loop {
        match stdin.read(&mut buf) {
            // EOF / error: stdin will never carry a reply — don't query.
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        if !crate::termquery::stdin_readable_within(std::time::Duration::from_millis(50)) {
            return true;
        }
    }
}

/// Non-unix fallback: no `poll(2)` to probe with, so keep the direct query (the
/// pre-probe behavior; crossterm's Windows console path answers via the API).
#[cfg(not(unix))]
fn terminal_answers_status_query() -> bool {
    true
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
