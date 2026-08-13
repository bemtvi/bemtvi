//! Toolkit-neutral machinery for `'imagepreview'` windows, shared by every
//! client that paints pixels: the remote (daemon-session) byte-fetch cache and
//! the bounded decode helpers. *Painting* stays per client — the TUI builds a
//! ratatui-image protocol, the GUI uploads a wgpu texture — but how the bytes
//! are obtained, versioned, and decoded is identical, so it lives here.

use std::collections::HashMap;
use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ImageReader};

/// Cap the longest edge of a decoded image (pixels) before handing it to the
/// client's paint layer. A preview never needs more than the screen can show, so
/// this bounds the retained copy and the per-resize/upload cost for a huge file —
/// the never-freeze guard — while staying generous enough that even a big /
/// hi-DPI surface looks crisp. (The full-resolution decode still happens once;
/// only the retained copy is shrunk.)
pub const MAX_EDGE: u32 = 2560;

/// Strict decode-time edge cap (pixels per side). The `image` crate's default
/// limits leave the *strict* dimension check off (only a best-effort 512 MiB
/// alloc cap, which some decoders ignore), so a crafted header — a
/// "decompression bomb": a tiny file whose dimensions claim a giant bitmap —
/// would drive a multi-hundred-MiB transient allocation before the downscale.
/// These caps fail the bomb at decode, before the allocator sees the claimed
/// size, while staying generous for real photos (8K is 7680×4320).
const MAX_DECODE_EDGE: u32 = 16384;

/// Strict decode-time allocation budget: the claimed
/// `width * height * bytes-per-pixel` must fit, or the decode is rejected.
/// (Non-strict in the crate's own terms — decoders honor it best-effort — so
/// the dimension check above is the real guarantee; this one bounds the rest.)
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// The decode-time [`image::Limits`] applied to every preview decode
/// (constructed via `Default` because the struct is `#[non_exhaustive]`).
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// A request to fetch a remote (daemon-session) preview's bytes over the editor
/// RPC. The client can't read the marker's `path` off its own disk — the file is
/// on the daemon — so [`RemoteImages::ensure_fetch`] emits one of these; the
/// client's event loop fulfils it with `bemtvi_image_read` and feeds the bytes
/// back via [`RemoteImages::deliver`].
pub struct ImageFetch {
    pub path: String,
    /// The preview's on-disk version `(size, mtime_ms)`, echoed back so a stale
    /// reply for a superseded version is dropped rather than replacing newer bytes.
    pub version: (u64, u64),
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

/// The out-of-band byte fetches for remote (daemon-session) previews, keyed by
/// path — the bytes aren't on local disk, so they're requested over the editor
/// RPC. The cache only tracks state; issuing the request is the caller's (it owns
/// the transport), via the [`ImageFetch`] that [`ensure_fetch`](Self::ensure_fetch)
/// hands back.
#[derive(Default)]
pub struct RemoteImages {
    slots: HashMap<String, RemoteSlot>,
}

impl RemoteImages {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make sure a fetch is in flight (or already resolved) for `path` at
    /// `version`, returning the [`ImageFetch`] to issue when a new request is
    /// needed. Emits exactly one request per (path, version): the slot dedupes
    /// re-requests while a fetch is pending and across frames, and a changed
    /// version (a watch reload) supersedes with a fresh request. The reply
    /// returns via [`deliver`](Self::deliver).
    #[must_use = "send the returned ImageFetch over the client's transport, or the fetch never happens"]
    pub fn ensure_fetch(&mut self, path: &str, version: (u64, u64)) -> Option<ImageFetch> {
        if self.slots.get(path).map(|s| s.version) == Some(version) {
            return None; // already fetching this version, or its bytes/failure are cached
        }
        self.slots.insert(
            path.to_string(),
            RemoteSlot {
                version,
                state: RemoteState::Pending,
            },
        );
        Some(ImageFetch {
            path: path.to_string(),
            version,
        })
    }

    /// The fetched bytes for a remote preview if they've arrived and still match
    /// `version`, else `None` (in flight, failed, or a superseded version).
    pub fn ready(&self, path: &str, version: (u64, u64)) -> Option<&[u8]> {
        match self.slots.get(path) {
            Some(RemoteSlot {
                version: v,
                state: RemoteState::Ready(bytes),
            }) if *v == version => Some(bytes),
            _ => None,
        }
    }

    /// Whether a fetch for `path` at `version` is still in flight (so the
    /// placeholder reads "loading" rather than "cannot read").
    pub fn is_loading(&self, path: &str, version: (u64, u64)) -> bool {
        matches!(
            self.slots.get(path),
            Some(RemoteSlot { version: v, state: RemoteState::Pending }) if *v == version
        )
    }

    /// Receive an `bemtvi_image_read` reply (routed from the client's event loop):
    /// cache the bytes, or mark the fetch failed, so the next paint decodes /
    /// falls back. A reply for a version no longer requested is dropped (a newer
    /// fetch superseded it). The caller repaints afterward.
    pub fn deliver(&mut self, path: String, version: (u64, u64), result: Result<Vec<u8>, String>) {
        if self.slots.get(&path).map(|s| s.version) != Some(version) {
            return; // a newer fetch (or a closed preview) superseded this reply
        }
        let state = match result {
            Ok(bytes) => RemoteState::Ready(bytes),
            Err(_) => RemoteState::Failed,
        };
        self.slots.insert(path, RemoteSlot { version, state });
    }

    /// Drop every slot whose path fails `keep` — a closed preview frees its
    /// fetched bytes.
    pub fn retain_paths(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.slots.retain(|k, _| keep(k));
    }

    /// Drop all fetched state (e.g. on a `:connect` session swap: the new
    /// session's files are unrelated to the old session's paths).
    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

/// Read and decode an image file into a [`DynamicImage`], guessing the format
/// from its contents (not just the extension). `None` on any read / decode
/// error. The retained copy is downscaled to `max_edge` (itself capped at
/// [`MAX_EDGE`]) so it stays bounded regardless of the source resolution.
pub fn decode_file(path: &str, max_edge: u32) -> Option<DynamicImage> {
    let mut reader = ImageReader::open(path).ok()?;
    reader = reader.with_guessed_format().ok()?;
    reader.limits(decode_limits());
    let img = reader.decode().ok()?;
    Some(downscale(img, cap(max_edge)))
}

/// Decode in-memory image `bytes` — a remote preview's bytes fetched over the
/// editor RPC (`bemtvi_image_read`), which the client can't read off its own
/// disk. Guesses the format from the contents like [`decode_file`] and applies
/// the same `max_edge` downscale. `None` on a decode error (a corrupt /
/// unsupported file → the placeholder).
pub fn decode_bytes(bytes: &[u8], max_edge: u32) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader = reader.with_guessed_format().ok()?;
    reader.limits(decode_limits());
    let img = reader.decode().ok()?;
    Some(downscale(img, cap(max_edge)))
}

/// The effective edge cap: the caller's limit (e.g. wgpu's
/// `max_texture_dimension`), never above [`MAX_EDGE`] and never zero.
fn cap(max_edge: u32) -> u32 {
    MAX_EDGE.min(max_edge.max(1))
}

/// Shrink `img` so its longest edge is at most `cap` (aspect-preserving), or
/// return it unchanged when it already fits — the shared tail of
/// [`decode_file`] / [`decode_bytes`].
fn downscale(img: DynamicImage, cap: u32) -> DynamicImage {
    if img.width().max(img.height()) > cap {
        img.resize(cap, cap, FilterType::Triangle)
    } else {
        img
    }
}

/// Decode an `bemtvi_image_read` reply into the bytes [`RemoteImages::deliver`]
/// expects: the binary payload on success, else a display string (an unexpected
/// reply shape, or the transport error) for the failed-fetch placeholder. Shared
/// by each client's fetch-fulfilment task.
pub fn image_read_reply<E: std::fmt::Display>(
    reply: Result<rmpv::Value, E>,
) -> Result<Vec<u8>, String> {
    match reply {
        Ok(rmpv::Value::Binary(bytes)) => Ok(bytes),
        Ok(other) => Err(format!("bemtvi_image_read: unexpected reply {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}
