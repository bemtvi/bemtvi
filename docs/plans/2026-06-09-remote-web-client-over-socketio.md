# Remote web client over Socket.IO

A phased plan to let the browser web client **also** connect to a real, full-featured
remote `nxvim-server` (Lua + treesitter + LSP) over Socket.IO — alongside the existing
fully client-side (serverless) WASM mode.

## Context

`nxvim-web` today is a **fully client-side** WASM build: `nxvim_core::Editor` compiled to
`wasm32-unknown-unknown`, driven by a `WebEditor` handle, rendered to HTML/CSS by
`web/index.html`, which does its **own** client-side tree-sitter highlighting because the
serverless core emits no highlights. There is no server, so Lua config, real treesitter,
and LSP are absent by construction (see architecture.md → *The web build*).

We want the web client to **also** connect to a real `nxvim-server` — without losing the
serverless mode. A new standalone Rust **bridge** binary spawns a local `nxvim-server`
stdio process per connection and relays its msgpack-RPC between the process and the
browser. Transport is **Socket.IO** (`socketioxide` on the bridge, `socket.io-client` in
the browser) rather than raw WebSocket, for reconnection / heartbeat / named-event framing.

Assume the `nxvim-server` stdio binary already exists (separate branch) — it exposes the
existing msgpack-RPC surface over stdin/stdout via `nxvim_server::run(stream, init)`.

### Key facts grounding the design

- **RPC framing** (`crates/nxvim-rpc/src/lib.rs`): msgpack arrays — Request
  `[0,id,method,params]`, Response `[1,id,err,res]`, Notification `[2,method,params]`.
  `reader_task` buffers partial frames and decodes with
  `rmpv::decode::read_value_with_max_depth` (short read ⇒ wait; other error / `> MAX_FRAME`
  ⇒ tear down). **tokio IO does not work on wasm**, so the browser client needs
  *synchronous* framing — it cannot use `nxvim_rpc::connect`.
- **The redraw decoder already exists and is wasm-clean** (`crates/nxvim-view/src/{view,parse}.rs`):
  `View::update(&[Value])` consumes one redraw notification's params (a single
  `Value::Map`) into a rich `View` (styles palette, per-row highlight spans
  `(start,end,group,style_id)`, chrome region styles, diagnostics underline/virt/signs,
  inlay hints, pmenu, panel, tabline, status segments, separators, windows). **redraw is a
  full frame each time.** It's currently dead-stripped in the wasm build; remote mode
  brings it to life.
- The server emits redraw as `rpc.notify("redraw", vec![Value::Map(map)])`
  (`crates/nxvim-server/src/redraw.rs`), matching `View::update`'s `params.first() => Map`.
- **Client protocol** (ref `crates/nxvim-tui/src/lib.rs`): `nvim_ui_attach(w,h,opts)`
  (request), notifications `nvim_input(notation)`,
  `nvim_input_mouse(button,action,mod,grid=0,row,col)`, `nvim_ui_try_resize(w,h)`,
  `nxvim_input_flush` (idle timeout flush armed after each keystroke), `nvim_command(cmd)`.
  The TUI replies `Ok(Nil)` to every server→client request.
- A **remote server has a real filesystem**, so `:e path` / `:w` work server-side — no File
  System Access API needed remotely (the GUI uses `nvim_command("e path")`/`"w"`).
- `nxvim-web` is workspace-**excluded** (targets wasm, built via `build.sh`) and pins its
  own deps. Existing host tests (`tests/clipboard.rs`, `tests/undo.rs`) link the rlib and
  use `WebEditor` — must keep compiling.

### Design decisions (locked)

1. Bridge = a new **Rust workspace-member** crate `nxvim-web-bridge` producing a
   **self-contained standalone binary**: the entire built JS frontend (`web/` —
   `index.html`, `pkg/*.{js,wasm}`, `tailwind.css`, `highlight.js`, `vendor/*`) is
   **embedded into the binary at compile time** (no `--root`, nothing served from disk).
   axum + `socketioxide` for transport; it spawns the `nxvim-server` binary as a child
   (one per connection) and byte-pumps stdio ↔ Socket.IO binary events.
2. **Full-fidelity** rendering: in remote mode the renderer paints the server's style
   palette + highlight spans + chrome + diagnostics + pmenu (not the JS highlighter).
3. Mode select: **auto-detect** (served by the bridge ⇒ remote, via `/config.json`) with
   **`?mode=local`** override to force serverless.
4. Transport: **Socket.IO** (binary events), not raw WebSocket.

---

## Phase 1 — WASM remote-client wire layer (`crates/nxvim-web`)

New module `src/remote.rs` (declared `mod remote;` in `lib.rs`), exporting a second
`#[wasm_bindgen]` type `RemoteClient` alongside the untouched `WebEditor` (both coexist in
one cdylib). It does **synchronous** msgpack-RPC framing — no tokio — reusing
`nxvim_view::View` for decode and `nxvim_view::{notation, encode_paste}` for input.

```rust
#[wasm_bindgen]
pub struct RemoteClient {
    view: View, inbuf: Vec<u8>, next_id: u64,
    width: usize, height: usize,
    dirty: bool, closed: bool, owed_response: Option<u64>,
}
```

**Outgoing encoders** — each msgpack-encodes the RPC array and **returns `Vec<u8>`**
(marshals to a JS `Uint8Array` the frontend hands to `socket.emit`): `new(w,h)`,
`attach() -> Vec<u8>` (request `nvim_ui_attach(w,h,{})`), `key(ctrl,alt,shift,name)`,
`input(notation)`, `paste(text)`, `input_mouse(button,action,modifier,row,col)`,
`command(cmd)`, `try_resize(w,h)`, `flush()` (notification `nxvim_input_flush`). Private
helpers `notify(method,params)`, `request(method,params)` (bumps `next_id`),
`encode(&Value)`. Factor `neutral_key` + the bare-`<S-…>` fixup out of `WebEditor::key`
into a shared free fn so both `key()` methods reuse it (the only refactor to existing code;
behavior-preserving).

**Incoming framing** — `feed(&[u8])` mirrors `reader_task`'s drain loop synchronously:
append to `inbuf`; loop decoding `Value` frames via `read_value_with_max_depth`
(`UnexpectedEof` ⇒ break/wait; other error ⇒ `closed=true`); `inbuf.drain(..n)` per frame;
`dispatch`; set `closed` if `inbuf.len() > MAX_FRAME`. Keep `MAX_DEPTH`/`MAX_FRAME` equal to
`nxvim-rpc`'s. `dispatch`: notification `[2,"redraw",[map]]` → `view.update(&params)`,
`dirty=true` (move params out with `mem::replace`, don't clone); response `[1,…]` → ignore;
request `[0,id,…]` → stash `owed_response=Some(id)`. Accessors `dirty()`, `closed()`,
`take_response() -> Option<Vec<u8>>` (encodes `[1,id,Nil,Nil]`, mirroring the TUI's reply).

**Rich serializer** — `view_json() -> String` clears `dirty` and hand-rolls
`rich_view_to_json(&View)` (no serde derives on `View`), a **superset** of the existing
`view_to_json` so the renderer shares most code, adding the server-only fields: global
`styles` palette (colors as `"#rrggbb"`/`null`), `chrome` region styles,
`global_status`/`tabline_segments`/`tabline`/`current_tab`, `panel`, `pmenu`, `separators`;
per window the existing fields plus `rect` (**nullable** — legacy flat redraw), `status`
segments + `status_visible`, and decorations `highlights` (`[[s,e,group,style_id|null]]`,
screen cols), `diagnostics`, `diagnostics_virt`, `diagnostics_signs`, `sign_column`,
`inlay_hints`. Style ids index `styles` (emit the index for per-cell decorations; inline
resolved styles for chrome/status). Omit `scroll` (web doesn't animate).

**Cargo**: add `rmpv = "=1.3.1"` to `crates/nxvim-web/Cargo.toml` (already transitive via
`nxvim-view`'s public API). Keep `remote.rs` free of `web-sys`/`js-sys` so it also builds on
the host target.

**Host test** `tests/remote.rs` (links the rlib): frame reassembly across a mid-frame
`feed` split; two frames in one `feed`; corrupt prefix ⇒ `closed()`; `view_json()` carries
styles/highlight/status; encoders decode back to the expected RPC arrays.

## Phase 2 — the Socket.IO bridge crate (`crates/nxvim-web-bridge`) — ✅ DONE

> **Status: implemented.** Pinned `axum =0.8.9`, `socketioxide =0.18.3`, `bytes =1.11.1`,
> `rust-embed =8.11.0`, `mime_guess =2.0.5`. The crate is `crates/nxvim-web-bridge`
> (`lib.rs` = the relay + router, `main.rs` = arg-parse/serve, `build.rs` = the release
> embed guard, `src/bin/stub_server.rs` = the test fixture). Two deviations from the
> sketch below, both deliberate:
>
> 1. **The child is `nxvim --server`, not a standalone `nxvim-server` binary.** The
>    headless server merged into `main` as the single `nxvim` binary's `--server` role
>    (see architecture.md → *Embedded vs. remote*), so the bridge spawns `nxvim --server`.
>    `$NXVIM_SERVER_BIN` points at the `nxvim` binary; the bridge appends `--server`.
> 2. **The relay is factored out of the socket handler as `relay_connection`** (a
>    transport-agnostic byte pump over an inbound channel + an `emit` callback). The
>    Socket.IO event handlers are a thin wrapper that feeds it. This made the pump
>    directly testable: `tests/bridge.rs` drives `relay_connection` against the real stub
>    child over real OS pipes (input forwarded → reply pumped back → reassembled across a
>    chunk split), plus HTTP-surface tests (`/config.json`, embedded `index.html`, 404)
>    over real TCP via `ureq`. **The Socket.IO wire is *not* tested in Rust:** the
>    `rust_socketio` 0.6 blocking client and `socketioxide` 0.18 don't interoperate at the
>    engine.io polling handshake (each is CI'd against the JS reference, not the other), so
>    the live socket round-trip is deferred to the Phase-5 Playwright E2E with the actual
>    browser `socket.io-client`. The bridge's own Socket.IO setup is verified by hand
>    (polling handshake → 200 + sid, ws upgrade → 101).

New **workspace member** (add to root `Cargo.toml` `members`; inherits
`[workspace.dependencies]`). New pins (confirm versions before pinning): `axum`,
`socketioxide`, `bytes`, **`rust-embed`** (embeds in release, reads disk in debug),
`mime_guess`; `tokio`/`anyhow` from the workspace. No `tower-http`/`ServeDir` — assets are
served from the embed.

**Embedded frontend** — the whole `web/` tree is baked into the binary:
```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "../nxvim-web/web"]
struct WebAssets;
```
An axum fallback handler resolves the request path against `WebAssets::get(path)` (default
`index.html`), returning bytes with a `Content-Type` from `mime_guess` (and
`application/wasm` for `pkg/*.wasm`). The result is one executable that needs nothing else
on disk to serve the UI.

`src/main.rs`:
- Args minimal: `--addr` (default `127.0.0.1:8000`). Locate the `nxvim-server` child via
  `NXVIM_SERVER_BIN` env → sibling of `current_exe()` → `PATH`; fail loud if missing.
- `SocketIo::new_layer()`; `io.ns("/", on_connect)`; axum `Router` with
  `.route("/config.json", get(|| Json(json!({"mode":"remote"}))))`, `.layer(socketio_layer)`,
  `.fallback(static_handler)`. The `/socket.io/` engine.io endpoint is owned by the layer.
- `on_connect(socket)`: spawn `tokio::process::Command::new(server_bin)` with piped
  stdin/stdout (one child per connection). **client→server**:
  `socket.on("rpc", |Data<Bytes>(b), stdin|)` → `stdin.write_all(&b)` + flush.
  **server→client**: a task reads raw chunks from `child.stdout` and
  `socket.emit("rpc", Bytes::copy_from_slice(chunk))` — **raw byte pump, no msgpack
  re-framing** (the client's `feed` reassembles). `on_disconnect` / child-exit kills the
  child and stops the pump (couple teardown like `nxvim-rpc`).
- **Binary** payloads both ways (msgpack ≠ UTF-8): `Data<Bytes>` in, `Bytes` out.

**Integration test**: start the bridge against a **stub** stdio child (a tiny binary
emitting a canned redraw), connect a Socket.IO client, emit an encoded `nvim_ui_attach`,
assert a binary `rpc` event returns and decodes via `rmpv`; assert chunked/partial
forwarding reassembles.

## Phase 3 — frontend dual mode + socket.io-client + rich renderer (`web/index.html`) — ✅ DONE

> **Status: implemented & verified end-to-end.** `web/index.html` now dual-boots
> (`detectMode()` → `bootLocal()` / `bootRemote()`); `RemoteClient` drives a Socket.IO
> socket; the rich render path (`renderLineServer` + `applyChrome`/`styleToCss`/
> `paletteStyle`, segment status/tabline/global-status, pmenu, sign column, nullable
> rect) paints the server's resolved palette + per-cell decorations. socket.io-client
> 4.8.1 is vendored into `web/vendor/socket.io/` (build.sh `build:socketio` step +
> `scripts/vendor-socketio.mjs`); `highlight.js` now exports `colorFor` as the un-themed
> highlight fallback. Verified via Playwright against the running bridge: remote boot +
> connect, `ihello world<Esc>` round-trips through the server and renders, mode
> transitions, `:e! <file>` opens a real server-side file, the segment status line and
> chrome palette paint, the toolbar is hidden and the header re-labeled; `?mode=local`
> still boots the serverless `WebEditor` unchanged. Two deviations from the sketch below,
> both forced by a real socket.io-client ↔ socketioxide interop gap discovered here:
>
> 1. **Transport is websocket-only** (`io({ transports: ["websocket"] })`). The HTTP
>    long-polling handshake between socket.io-client 4.8.1 and the bridge's engineioxide
>    drops the session immediately after the open handshake ("Session ID unknown" on the
>    first follow-up poll), so the default polling-first connect never establishes. The
>    direct websocket upgrade works cleanly. ws is also the better transport for a binary
>    RPC stream (no base64 polling overhead, lower latency); socket.io still provides
>    reconnection + heartbeat + the named `"rpc"` event on top.
> 2. **RPC frames cross the wire as base64 *text* events, not binary** (revises the
>    locked "binary frames only" decision and risk #3). socket.io-client emits a binary
>    attachment whose placeholder socketioxide never pairs over the ws transport
>    (`Data<Bytes>` fails with "expected a binary placeholder"); string events deliver
>    fine. So the bridge (`base64 =0.22.1`, new pin) base64-decodes each inbound `"rpc"`
>    string to bytes for the child's stdin and base64-encodes each stdout chunk back; the
>    browser does the same with `btoa`/`atob` helpers around `RemoteClient`. The
>    `relay_connection` byte-pump and its tests are unchanged — only the socket boundary
>    in `on_connect` encodes/decodes.
>
> The original implementation sketch follows.

**Vendor `socket.io-client`** — add to `package.json` devDeps and a `build.sh` step copying
its dist bundle into `web/vendor/` (gitignored like the tree-sitter vendor + `pkg`), so no
runtime CDN. Import it in `index.html`.

**Mode select / boot** — read `?mode=local`; detect remote via `fetch('/config.json')` →
`mode === "remote"` (absent ⇒ local). Split `boot()` into `bootLocal()` (today's path
verbatim: `WebEditor` + JS tree-sitter) and `bootRemote()`: `await init()` (wasm still
needed for `RemoteClient`); construct `RemoteClient`; `ts=null`; `const socket = io()`;
`socket.on("connect", () => socket.emit("rpc", client.attach()))`;
`socket.on("rpc", buf => { client.feed(new Uint8Array(buf)); if (client.closed()) {…}
const r = client.take_response(); if (r) socket.emit("rpc", r);
if (client.dirty()) { view = JSON.parse(client.view_json()); render(); resolvePendingRedraw(); } })`.
Reconnection is handled by socket.io.

**Input shims** — `sendKey`/`sendPaste`/`sendInput`/`sendMouse`/`sendCommand`/`sendResize`
so handlers don't branch everywhere. Remote ⇒ `socket.emit("rpc", client.X(...))` then
`armFlush()` (key/paste only); the server's redraw round-trips and re-renders in the `"rpc"`
handler — handlers do **not** render synchronously. Local ⇒ existing `editor.X` +
`refresh()`. Timeout flush mirrors the TUI's `flush_armed`: a JS `setTimeout(1000)` reset
on each input, firing `client.flush()`. Clipboard ferrying is local-only — guard behind the
`editor` handle (the server owns `"+`/`"*`).

**Rich render path** — key on `serverStyled = view.styles?.length > 0`. New helpers
`styleToCss(style)`, `paletteStyle(id)`, `applyChrome()` (drive `#grid` bg/fg + gutter /
status / eob from `view.chrome`). `render()` skips `highlightContext()`/`ts` when
`serverStyled`. `renderLine` colors **by screen column** from `w.highlights[row]` (palette →
CSS, else a small group→color fallback), layers selection/search/incsearch/cursor as today,
then overlays diagnostics (wavy underline), **inlay_hints** (insert virtual cells, shifting
real glyphs right — assemble the final ordered cell list first, then layer classes by final
column; the subtlest detail — verify against a TUI render), and `diagnostics_virt`.
`renderGutterCell` colors from chrome and reserves a sign column when `w.sign_column`.
`renderStatus` paints `w.status` segments (skip when `status_visible===false`); add
`renderGlobalStatus` for `laststatus=3`. `renderTabline` paints `tabline_segments` when
present. New `renderPmenu(view.pmenu)`. `renderWindow`/`renderFloat` handle `rect === null`.
Local mode keeps the JS-color path unchanged.

## Phase 4 — file ops & UX in remote mode

- A remote server has a real fs: `:e path` / `:w` / `:wq` are ordinary keystrokes that
  reach the server as `nvim_input`. **Disable `maybeHandleCommand` interception remotely**
  so `<CR>` flows to the server; no File System Access API path remotely (keep for local).
- Toolbar: hide the local Open/Save buttons and the FS-Access notice remotely; branch the
  header text ("connected to remote nxvim-server" vs "runs entirely in your browser").
- Out of scope for v1 (flag): browser↔server clipboard integration (the server's clipboard
  is the host's, not the browser user's).

## Phase 5 — verification (end to end)

- **Host unit** (`cargo test -p nxvim-web` via the rlib) — `tests/remote.rs` from Phase 1.
- **wasm build** (must precede a *release* bridge build — assets are embedded):
  `crates/nxvim-web/build.sh` emits `web/pkg/`, `web/tailwind.css`, `web/vendor/`; confirm
  `web/pkg/nxvim_web.js` exports both `WebEditor` and `RemoteClient`, and `web/vendor/` has
  socket.io-client. Debug bridge reads `web/` from disk (fast loop); release bakes it in.
- **Bridge integration** — `cargo test -p nxvim-web-bridge` against a stub stdio child.
- **Headless E2E** (Playwright) — run `build.sh`, then `NXVIM_SERVER_BIN=<path> cargo run -p
  nxvim-web-bridge`, navigate to `http://127.0.0.1:8000/`. A remote `window.__nxvim` hook
  mirrors the local one: `feed(notation)`/`key`/`mouse`/`command` emit via the client;
  `view()` returns the last decoded rich view; `nextRedraw()` resolves after the next
  applied redraw — a deterministic "send → await server redraw → assert" loop. Assert
  `await __nxvim.feed("ihello<Esc>"); await __nxvim.nextRedraw()` ⇒
  `view().windows[0].lines[0] === "hello"` and `view().styles.length > 0`; a
  `:w /tmp/out.txt<CR>` writes server-side; a known token's rendered cell carries a
  non-default color. Re-run with `?mode=local` (regression guard). Screenshot both modes.

## Risks / watch-items

1. **Inlay-hint column interleaving** in `renderLine` — verify against a TUI render of an
   identical frame.
2. **Status is segment-based remotely** — the local synthesizer relies on fields
   `WindowView` lacks; keep the two paths separate.
3. **Binary frames only** across Socket.IO — enforce `Data<Bytes>` / binary emits /
   `Uint8Array` on both ends.
4. **`MAX_FRAME`/`MAX_DEPTH` parity** with `nxvim-rpc`.
5. **Server→client requests** — include `take_response()` now to avoid a silent hang.
6. **New workspace dep pins** must be exact (`=x.y.z`) per repo convention.
7. The new `remote.rs` must compile on the **host** target — no `web-sys`/`js-sys`.
8. **Build ordering** — a release bridge embeds `web/` at compile time, so `build.sh` runs
   **before** `cargo build --release -p nxvim-web-bridge`. Optionally a `build.rs` errors
   clearly if `../nxvim-web/web/pkg` is missing in a release build.

## Critical files

- `crates/nxvim-web/src/remote.rs` — **NEW** `RemoteClient` (sync framing, encoders, rich
  `view_json`).
- `crates/nxvim-web/src/lib.rs` — declare `mod remote`; factor out `neutral_key`/shift fixup.
- `crates/nxvim-web/Cargo.toml` — add `rmpv = "=1.3.1"`.
- `crates/nxvim-web/web/index.html` — dual boot, socket.io-client, send shims, flush timer,
  rich server-styled render path, remote `__nxvim` hook.
- `crates/nxvim-web/{package.json,build.sh}` — vendor socket.io-client into `web/vendor/`.
- `crates/nxvim-web-bridge/{Cargo.toml,src/main.rs}` — **NEW** standalone binary:
  socketioxide transport + `rust-embed` of `../nxvim-web/web` + child-process byte pump.
- `Cargo.toml` (root) — add `nxvim-web-bridge` member; pin
  axum/socketioxide/bytes/rust-embed/mime_guess.
- Reference only (no change): `crates/nxvim-view/src/{view,parse,style}.rs`,
  `crates/nxvim-rpc/src/lib.rs`, `crates/nxvim-tui/src/lib.rs`,
  `crates/nxvim-server/src/redraw.rs`.
