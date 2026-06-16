# Image previews (TUI-first)

Opening an image file shows the actual picture instead of its bytes, gated by an
option. This doc is the phased plan; work one phase at a time, commit + pause for
review between phases.

## Goal & UX

- `nx.o.imagepreview = true` (off by default) turns the feature on.
- With it on, `:e photo.png` (and the CLI arg, eventually) opens the file as an
  **inert preview buffer** — the picture is rendered; the bytes are *not* loaded
  as text. This is the "open = preview" model (cf. vim/image.nvim), confirmed with
  the requester.
- With it off, an image file opens as it does today (bytes decoded through
  `'fileencodings'`, shown as text).

## The architectural problem

nxvim is a headless server + thin text-grid clients. The whole pipeline is cells:
buffers are ropes, the `View` projects to cell-coordinate spans, the redraw
notification is msgpack text+spans, and every client (ratatui TUI, wgpu GUI,
canvas web) paints a monospace grid. Nothing carries or draws a raster. So "show
an image" = thread a new *non-text content kind* from the server through the
redraw protocol out to a client that knows how to render it.

Two invariants this must respect:
- **Editor must never freeze** — the redraw frame carries a *reference* (path),
  never the image bytes. Base64-ing a multi-MB image into every redraw frame is
  the per-event-work-scales-with-output anti-pattern. Bytes are read/decoded once,
  client-side, and cached.
- **Dogfood the nx API** — the policy/gating is the `nx.o.imagepreview` option;
  the protocol + per-client rendering are legitimately Rust frame work (Lua can't
  blit pixels).

## Tooling decisions (settled)

- TUI renderer: **`ratatui-image` `=11.0.4`**, `default-features = false`,
  `features = ["image-defaults", "crossterm"]`. **No chafa** (the default
  `chafa-dyn`/`chafa-static` only improve the dumb-terminal halfblock *fallback*
  and drag in a C lib via pkg-config; Kitty/Sixel/iTerm2 are pure-Rust and
  unaffected).
- **ratatui pin bump `=0.30.0` → `=0.30.1`** (shared `[workspace.dependencies]`):
  ratatui-image 11.0.4 requires `ratatui ^0.30.1`, so the old exact `=0.30.0` pin
  cannot resolve it. 0.30.1 is a semver patch; the whole workspace recompiles and
  the test suite confirms no regression.

## Data design

A window's image is a *reference*, carried per-window:

- core `nxvim_core::view::WindowView` gains `image: Option<ImageView>` where
  `ImageView { path: String }` (Phase 1). Phase 2 may add `size`/`mtime_ms` for a
  precise client cache key if statting client-side proves insufficient.
- `redraw.rs` emits it as a per-window sub-map `{"image": {"path": …}}` (or `Nil`
  when absent — older/none clients ignore it).
- wire `nxvim_view::WindowView` gains the mirror `image: Option<ImageData>`.

The buffer marks itself: `Buffer::image: bool`, set by `Buffer::from_image_file`
(stats the file for its disk snapshot but does **not** read the bytes — the rope
stays the empty `"\n"`). The buffer is still a valid, named, unmodified buffer, so
the status line names it and a stray `:w` has a target.

## Phases

### Phase 1 — option + policy + protocol (pure core, no rendering) — TESTABLE

The whole bool-option surface for `imagepreview`:
- `options.rs`: `Options.imagepreview` field + default `false`; `:set` name table
  entry `"imagepreview" => Bool`.
- `editor/options.rs`: `apply_set_bool` arm.
- `editor/windows.rs`: `set_global_option_bool` arm (the `vim.o`/`nx.o` write seam).
- `nxvim-lua/runtime.rs`: `GoMirror.imagepreview` field.
- `nxvim-server/effects.rs`: `GoMirror { … imagepreview: go.imagepreview }`.
- `nxvim-lua/prelude/state.lua`: option name-map + defaults entries.

Image policy + plumbing:
- `editor/mod.rs`: `is_image_path(path) -> bool` (extension table, sibling of
  `language_of_path`).
- `buffer.rs`: `Buffer.image` field + `from_image_file()`; update the three
  exhaustive constructor literals.
- `editor/buffers.rs`: `read_buffer()` helper that picks `from_image_file` vs
  `from_file` by option+extension; route the three local-FS load seams
  (`load_into_current`, `reload_buffer`, `load_new_buffer`). (Off-tick/daemon and
  CLI-at-construction opens don't preview yet — Phase 3.)
- `view.rs`: `ImageView` + `WindowView.image`, projected in `window_view()`.
- `redraw.rs`: emit the `image` sub-map.
- `nxvim-view/view.rs`: `ImageData` + parse.
- Bump ratatui to `=0.30.1`.

Test (black-box harness): with `imagepreview` on, `:e <tmp>.png` yields a redraw
window carrying the `image` marker (path) **and** an empty buffer (bytes not
loaded as text); with it off, the same file loads as text and carries no marker.

Commit, pause for review.

### Phase 2 — TUI rendering

- Add `ratatui-image` to `nxvim-tui`.
- Build a `Picker` once at startup (after alt-screen) — detects protocol + cell
  pixel size. Hold a path-keyed cache of `(DynamicImage, StatefulProtocol)`;
  decode-from-disk once, re-encode only on rect change.
- In the render path, a window with `image: Some(..)` renders `StatefulImage` into
  its text-body rect instead of the (empty) `lines`.
- Ship `examples/image-preview/` (config + a sample image), verified end-to-end.
  Pixel output isn't harness-assertable (no real terminal) → requester verifies.

Commit, pause for review.

### Phase 3 — polish

- Graphics-protocol redraw artifacts (clear/repaint on scroll), large-image
  downscale-on-decode, `ThreadProtocol` if synchronous re-encode stutters.
- CLI-open ordering so `nxvim photo.png` previews when config set the option
  (today the CLI buffer is built at `Editor` construction, before user config
  runs).
- Later/optional: GUI (wgpu texture) and web (`<img>`/canvas + out-of-band byte
  fetch for the remote/daemon case) renderers reusing the same `image` marker.
