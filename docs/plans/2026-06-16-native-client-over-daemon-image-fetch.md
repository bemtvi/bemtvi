# Native-client-over-daemon image fetching for the preview

**Status:** Complete (all four phases landed).

## Problem

`'imagepreview'` opens an image file as an inert preview buffer carrying a per-window
`image` marker `{path, size, mtime_ms}` through the redraw — a *reference*, never the
bytes (the never-freeze invariant). Each native client (TUI/GUI) reads and decodes the
file itself from the path, via `ImageReader::open(path)`:

- TUI: `crates/bemtvi-tui/src/images.rs::decode`
- GUI: `crates/bemtvi-gui/src/images.rs::decode`

This assumes the client shares the filesystem with the buffer. In a **daemon
(`:connect` / edit-host split) session** that assumption is false: the editor (and so
the redraw, the path) runs **local**, but the file's bytes live on the **remote
daemon** — the server's `host_fs_async` is daemon-backed. `ImageReader::open(path)`
then opens the wrong local file or (usually) fails, so the preview is blank/broken.

The web client already solved the equivalent problem out-of-band: it fetches the bytes
over the daemon (`worker.mjs::daemonRead` → `fs_read`) rather than off a shared disk.
This plan brings the same out-of-band fetch to the **native** clients.

## Key architectural fact

In *every* native topology (GUI/TUI, embedded or daemon), the editor server runs
**locally in-process**; the client talks to it over the editor-RPC duplex. The server's
`host_fs_async` is daemon-backed in a remote session and `None` (local disk) in an
embedded one. So the clean place to read remote bytes is **the local server**, through
its own `host_fs_async` seam — exposed to the client over a new editor RPC. The client
fetches only when the marker says the bytes are remote.

This keeps the fast local path (synchronous local decode) untouched and only pays the
async round-trip when the file is actually remote.

## Design

1. **Server stamps `remote` on the marker.** `redraw.rs` already projects `win.image`
   into the redraw map. Add `remote = self.host_fs_async.is_some()`. The wire type
   `bemtvi_view::ImageData` gains a `remote: bool` parsed in `from_redraw`/`update`.
   (Core's `ImageView` stays unchanged — core knows nothing of the daemon; `remote` is
   purely a redraw-projection concern.)

2. **New RPC `bemtvi_image_read [path] -> bin`.** Reads the file through the server's
   `host_fs_async` (daemon) — or local disk when `None` — and responds with the raw
   bytes (`Value::Binary`) or a loud error string. It is **async with a deferred
   response**: `dispatch` is synchronous, so `handle` intercepts this method, clones the
   cloneable `Rpc` handle, spawns a task that reads + `respond(id, …)` off-tick. Mirrors
   the existing off-tick `fs_fetch` pattern. A `New`/`Dir` result, or a read error, is an
   `Err` (the client shows its `[image: …]` placeholder).

3. **Native clients fetch when `remote`.** When `image.remote` is true, the client does
   not `ImageReader::open(path)`; instead it requests `bemtvi_image_read` over the editor
   RPC, decodes the returned bytes from memory (`ImageReader::new(Cursor::new(bytes))`),
   and feeds them into its existing path-keyed cache. While the fetch is in flight it
   paints the `[image: …]` placeholder, and on arrival it requests a repaint — exactly
   the web client's out-of-band shape. Version `(size, mtime_ms)` keys the cache the same
   way, so a watch-reload re-fetches.

## Phases

- **Phase 1 — server (this commit).** `remote` bit on `ImageData` + redraw projection;
  `bemtvi_image_read` RPC reading via `host_fs_async`. Tests (headless, via the daemon-fs
  harness): a daemon session marks `remote = true` and `bemtvi_image_read` returns the
  daemon's bytes; a local session marks `remote = false` and the RPC reads local disk.

- **Phase 2 — GUI.** Async fetch pipeline feeding the wgpu texture cache: a render-thread
  request channel → IO-thread `rpc.request` → `UserEvent::ImageBytes { path, version,
  bytes }` back to winit → `ImageStore` decodes-from-memory + uploads. Placeholder while
  pending; re-fetch on version change. (Pixels aren't agent-verifiable —
  `[[gui-window-not-screencapturable-from-agent]]`; verify build + logic.)

- **Phase 3 — TUI.** Async fetch via the single-threaded select loop: a spawned
  `rpc.request` task delivers bytes back on a channel the loop selects on, then redraws;
  `ImageStore` decodes-from-memory into the ratatui-image cache.

- **Phase 4 — docs/example/memory.** Update `ImageData` doc-comments (no longer "always
  local"), refresh the image-preview example/verify notes, update the memory note.

## Test seams

- Daemon-fs harness: `crates/bemtvi-server/tests/daemon_fs.rs` shows
  `spawn_with_daemon_fs` (a `RemoteHostFs` over a `serve_fs_daemon`/`DaemonFs` duplex).
  Phase 1's image test reuses this shape.
- Redraw marker assertions: `window0_field(m, "image")` + `map_get` (see
  `tests/image_preview.rs`).
