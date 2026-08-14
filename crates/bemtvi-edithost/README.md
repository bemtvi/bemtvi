# bemtvi-edithost — the wasm edit-host

The real editor in the browser: the reusable synchronous [`EditHost`] tick (bemtvi's
core + the PUC Lua 5.4 VM + the `vim.*` bindings + the full server glue — autocmds,
mirrors, lifecycle, the redraw projection) compiled to `wasm32-unknown-emscripten`,
driven behind a wasm [`HostEffects`] (`WasmEffects`) inside a Web Worker. This is Phase 5
of
[`docs/plans/2026-06-09-edit-host-and-browser-lua.md`](../../docs/plans/2026-06-09-edit-host-and-browser-lua.md).

It links the **production** `bemtvi-server` tick (the keystroke path is the same one the
native server drives — not a hand-wired minimal tie-in).

## What runs here vs. not (serverless, or a daemon over WebTransport)

The browser build runs **serverless by default**: no daemon, so anything that needs a
*process* is unavailable and fails *loud*, never silently. Files, however, persist —
they live in the browser's Origin Private File System (OPFS, Phase 6a).

**Optional daemon mode (Phase 6b):** opening the page with
`?daemon=bemtvi://HOST:PORT/TOKEN?cert=HASH` (the connect string a `bemtvi --daemon --listen`
prints) makes `:e` / `:w` / `:e <dir>` operate on the **daemon's** filesystem over a real
**WebTransport (HTTP/3 / QUIC)** connection instead of OPFS — the browser twin of the native
daemon fs leg, editing still entirely in the Worker (only fs crosses the wire). The off-tick
seam is identical; only the transport differs (OPFS ↔ the `web/rpc.mjs` msgpack-RPC client
over a bidi stream). Config + shada stay **local** (OPFS) even in daemon mode. The daemon
fs, watch, async-process (`vim.system` / `jobstart`), and terminal (`:terminal`) legs over
WebTransport have landed; LSP and `luafs` over the wire are later slices — so a daemon
session still fails LSP loud for now.

The serverless capability map (the default; daemon mode adds remote files + the legs above):

- ✅ Core editing + the full Lua VM + autocmds + the redraw projection.
- ✅ **Files via OPFS** (Phase 6a): `:e` / `:w` open/save real files in the browser's
  Origin Private File System, so edits survive a reload. OPFS handle acquisition is
  *async* (only a `FileSystemSyncAccessHandle`'s operations are sync), so it can't back
  the *synchronous* core `HostFs` without Asyncify — instead the editor runs in **off-tick
  fs** mode (`has_remote_fs() == true`) and the Worker fulfills `:e`/`:w` against OPFS
  *between* ticks (`eh_take_fs_requests` → `eh_fs_read_complete` / `eh_fs_write_complete`),
  the same off-tick seam a daemon session uses, only the transport is OPFS. `:e <dir>`
  lists a real OPFS directory (the netrw-style explorer) and opening an entry reads it
  back — the directory enumeration rides the same off-tick read reply (`kind == 2`).
- ✅ **Single-file `init.lua` from OPFS.** On boot the Worker reads `/init.lua` from OPFS
  (if present) and sources it through the real effects path, *between* the two halves of
  startup (`eh_new` does `boot_begin`; `eh_boot_finish` fires the lifecycle + sets
  `v:vim_did_enter`) — so the config sources first, then the startup buffer's autocmds
  (`BufEnter` …) fire, matching native ordering. Options, keymaps, autocmds, user
  commands, and highlights all apply. A broken config is surfaced (non-fatal); the editor
  still boots. **One self-contained file only** — `require` of further modules / plugins
  won't resolve, since the browser build's runtimepath is empty and OPFS reads are async
  (Lua's `require` is synchronous). Multi-file/plugin configs are a later step.
- ⚠️ Processes — the *synchronous* `vim.fn.system` / `btv._system` always fail loud with a
  named "not available in the browser build yet" (`WasmBlockingSystem`), since there is no
  blocking shell-out seam in the browser. The *async* spawn path (`vim.system` / `jobstart`)
  fails loud when **serverless**, but in **daemon mode** it runs over WebTransport (Phase 6d):
  the spawn crosses to the daemon, whose `proc_spawned` / `proc_exited` pushes land back in
  the tick. `:terminal` likewise runs against a connected daemon (Phase 7).
- ⚠️ **Syntax highlighting + tree-sitter** — highlighting is done JS-side in the UI thread
  via web-tree-sitter (`web/highlight.js` + the `web/vendor/` grammars), the Worker shipping
  the focused buffer's text with each frame and the page parsing + coloring it; **tree-sitter
  indentation** routes synchronously through the `eh_js_ts_*` bridge to the Worker's indenter
  (`web/ts-indent.js`). `:TSInstall <lang>` works: it fetches a prebuilt grammar `.wasm` +
  queries (offline bundle / OPFS cache / jsDelivr), registers it with the JS highlighter, and
  caches it in OPFS. **Native** treesitter (the in-process `bemtvi-ts` parser) is gated off the
  build; **LSP** is unavailable and fails loud (a later daemon slice would re-enable it).

## Interop

emscripten `ccall`/`cwrap` over the `#[no_mangle] extern "C"` exports in `src/lib.rs`
(`eh_new` / `eh_input` / `eh_input_mouse` / `eh_source_lua` / `eh_boot_finish` /
`eh_attach` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` / the OPFS fs leg
`eh_take_fs_requests` / `eh_save_bytes` / `eh_save_len` / `eh_fs_read_complete` /
`eh_fs_write_complete` / `eh_free*`) — **not** wasm-bindgen. The
redraw comes back as a JSON return value. Slice 5c runs these exports **inside a Web
Worker** and ferries the JSON redraw to the UI over `postMessage`. Slice 5d drives the
Worker's run loop off a `SharedArrayBuffer` + `Atomics.wait` park: the same wait that
blocks on input also fires Worker-side timers (`vim.defer_fn` / `btv.timer`) via
`eh_set_clock` / `eh_next_deadline` / `eh_tick_timers` — one mechanism, no Asyncify.

## The browser shell — `web/`

- `web/worker.mjs` — the Web Worker, the single `!Send` thread that owns core + Lua. It
  loads `dist/eh.mjs`, constructs the real `EditHost`, and drives the production tick.
  When the page is cross-origin isolated (5d) it runs a blocking loop parked on
  `Atomics.wait` against an SAB input ring, waking on a keystroke **or** the next timer
  deadline; otherwise (5c) it is `postMessage`-driven (input works, timers don't fire).
  The latest `redraw` frame + buffer lines post back to the UI either way.
- `web/index.html` — the UI thread: the **DOM renderer** (a per-cell-span renderer —
  windows/gutter/status/tabline/panel/pmenu, selection + cursor-shape
  classes, smooth scroll), driven off the server `redraw` frame; it translates keystrokes
  to vim key-notation and mouse gestures to `eh_input_mouse`, picks the SAB or postMessage
  transport by capability, and exposes `window.__bemtvi` (`feed` / `mouse` / `execLua` /
  `attach` / `lines` / `frame` / `cursor` / `cmdline` / `sab` / …) for automation.
  Keys the browser reserves for itself are the one thing it can't simply forward:
  Chrome/Edge on Windows and Linux handle `<C-w>` / `<C-t>` / `<C-n>` / `<C-Tab>` /
  `<C-1>`..`<C-9>` ahead of the page, so `preventDefault()` has no say and the editor never
  sees them. macOS puts those on Cmd, leaving Ctrl free — which is why `<C-w>` works there
  and only there. So on non-Mac platforms **Alt stands in for Ctrl on exactly those chords**
  (`Alt+w` → `<C-w>`, `Alt+Shift+W` → `<C-W>`); every other Ctrl chord is deliverable and
  untouched, and on macOS nothing is remapped — Ctrl already arrives there, and Alt is
  Option, the character-composing key. Two consequences worth knowing: `<A-w>` / `<A-t>` /
  `<A-n>` / `<A-1>`..`<A-9>` are unreachable in the browser (they now encode as `<C-…>`,
  while every other `<A-…>` — `<A-c>` for multi-cursor, say — is untouched), and `Alt+Tab`
  is claimed by the OS on both Windows and Linux, so `<C-Tab>` has no stand-in in practice.
- `web/highlight.js` — the client-side web-tree-sitter highlighter; its grammars/runtime
  are generated into `web/vendor/` by `build.sh` (gitignored). The import is optional —
  the renderer degrades to plain text if absent.
- `web/serve.mjs` — a cross-origin-isolated dev server (COOP/COEP/CORP), so the page can
  use a `SharedArrayBuffer`. `crossOriginIsolated === true`.
- `web/rpc.mjs` — the browser↔daemon msgpack-RPC client (Phase 6b): the JS twin of
  `bemtvi-rpc`, over one WebTransport bidi stream. `dialDaemon(uri)` connects (token on the
  CONNECT path, cert hash pinned TOFU); the Worker uses it to fulfill `:e`/`:w` over the wire
  in daemon mode. msgpack is the vendored `@msgpack/msgpack` (`web/vendor/msgpack/`, staged by
  `build.sh`); its `decodeMultiStream` frames the bidi stream.
- `web/verify.mjs` — the Playwright verifier: drives the real wasm edit-host in a real
  headless Chromium through `window.__bemtvi` and asserts buffer / cursor / redraw, **and**
  that a deferred timer fires unattended via the SAB park (plus the OPFS round-trip).
- `web/verify-ui.mjs` — the renderer/input verifier: asserts the DOM renderer (not a
  `<pre>`), the command-line-below-status-line layout, visual-mode selection painting,
  cursor-shape-by-mode, mouse click / drag-select / wheel-scroll, and tree-sitter
  highlight colors — all in a real headless browser.
- `web/verify-alt-ctrl.mjs` — the reserved-chord verifier: drives **real** keydown events
  (not the `feed` hook, which bypasses the encoding under test) and asserts `Alt+w` feeds
  `<C-w>` on a non-Mac platform, that an unreserved `Alt+c` still reaches the editor as
  `<A-c>`, and that a spoofed macOS session remaps nothing.
- `web/verify-config.mjs` — the config verifier: writes an `/init.lua` to OPFS, reloads,
  and asserts the config applied on startup (an option, a global, a keymap that fires,
  and a startup-buffer `BufEnter` autocmd) and that a broken config is non-fatal.
- `web/verify-daemon.mjs` — the daemon verifier (Phase 6b): spawns a real
  `bemtvi --daemon --listen`, opens the page with `?daemon=<uri>`, and asserts `:e`/`:w`/`:e
  <dir>` round-trip over WebTransport — the saved bytes read back from the daemon's disk in
  Node (so they truly crossed the wire), `[+]` clearing only on the daemon's ack. The browser
  twin of native `daemon_quic.rs`. (Needs `cargo build -p bemtvi` for the daemon binary.)
- `web/verify-connect.mjs` — the runtime-`:connect` verifier: spawns a real
  `bemtvi --daemon --listen`, opens the page **serverless** (no `?daemon=`), then dials the
  daemon at runtime by typing `:connect bemtvi://…` and pressing Enter through the real keydown
  interception — asserting a subsequent `:e <daemon file>` reads the daemon's bytes over the
  wire (the off-tick fs seam re-pointed from OPFS), and that a non-`bemtvi://` URI is rejected
  loudly. The browser twin of bemtvi-gui's client-side `:connect`. (Needs the daemon binary.)

> **Note (not a gap, and not wasm-specific):** the Lua `vim.api.nvim_buf_*` *mutation*
> surface (`nvim_buf_set_lines` / `set_text` / `set_name`, `nvim_open_win`,
> `nvim_create_buf`, `nvim_feedkeys`, …) reads as `nil` here — but **by design, in every
> build**: bemtvi's config API is autocmds / diagnostics / keymaps / options, not entity
> mutation (see `crates/bemtvi-lua/src/prelude/api.lua`'s header). The *read* getters
> (`nvim_buf_get_lines`, …) and extmarks do exist. Mutate a buffer via keystrokes /
> ex-commands / `vim.cmd`.

## Build & run

There are **two builds** from this one source tree (see
[`docs/plans/2026-06-23-web-python-demo.md`](../../docs/plans/2026-06-23-web-python-demo.md)):

- **the standard editor** — `./build.sh` → `dist/` + `web/`. No Pyodide;
  `web/build-config.js` keeps `localHost: false`, so the Worker never installs the local
  process host (`:terminal` needs a daemon, as before).
- **the python demo** — `./build-demo.sh` → a self-contained `demo-site/` (gitignored): the
  standard build PLUS Pyodide (CPython → wasm) vendored in and `build-config.js` flipped to
  `localHost: true`, so a serverless `:terminal python <file>` runs CPython in the browser.
  Serve it with `BEMTVI_SERVE_ROOT=demo-site node web/serve.mjs`; verify with
  `node verify-pyodide-terminal.mjs`.

```sh
rustup target add wasm32-unknown-emscripten   # once
# plus emcc: an installed+activated emsdk, or the system emscripten package
./build.sh         # → dist/eh.mjs + eh.wasm  (standard editor)
./build-demo.sh    # → demo-site/  (the python demo: Pyodide + the local process host)
node harness.mjs   # node smoke test: feeds `ihello<Esc>`, asserts lines + a real redraw

# the browser shell:
cd web && npm install        # playwright (once); plus `npx playwright install chromium`
node serve.mjs               # open http://localhost:8088/web/  to use the editor
node verify.mjs              # headless-browser proof of the editor/transport/OPFS contract
node verify-ui.mjs           # headless-browser proof of the renderer/mouse/selection/highlighting
node verify-config.mjs       # headless-browser proof of single-file init.lua sourcing from OPFS
node verify-daemon.mjs       # headless-browser proof of :e/:w/:e<dir> over WebTransport to a real --daemon
node verify-connect.mjs      # headless-browser proof of runtime `:connect bemtvi://…` (no ?daemon= param)
node verify-fs.mjs           # headless-browser proof of the real local-FS picker (:eo/:wo/bare :w)
node verify-luafile.mjs      # headless-browser proof of the Lua-source picker (:luafile/:source)
node verify-http.mjs         # headless-browser proof of serverless btv.http.fetch (browser fetch())
node verify-http-mount.mjs   # headless-browser proof of btv.http.mount (the Service Worker leg)

# daemon mode by hand: start a daemon, then either open the page with its connect URI…
#   cargo run -p bemtvi -- --daemon --listen 127.0.0.1:8765   # prints bemtvi://…?cert=…
#   open  http://localhost:8088/web/?daemon=<that-bemtvi://-uri>
# …or open the page serverless and dial it at runtime with  :connect <that-bemtvi://-uri>
```

**Configuring it:** drop a single self-contained `init.lua` at the OPFS root (`:w
/init.lua` from inside the editor, or write it via the File System API) — on the next
load the Worker sources it at startup. Options / keymaps / autocmds / user commands /
highlights apply; `require` of further modules / plugins does not (empty runtimepath).

**Trying a config without a reload:** `:luafile` (aliases `:source` / `:luao`) pops the
native file picker for a real local `.lua` file and runs it through the live effects path
— the browser twin of vim's `:luafile <file>`. This is the quick way to try the
repo's `examples/*/init.lua` configs: `:luafile`, pick the example's `init.lua`, and (if
it ships a sample) `:eo` its sample file. Because it runs *after* boot, autocmds the
config registers fire on subsequent events, not retroactively for the already-open
buffer; same single-file `require` caveat as the OPFS `init.lua`. Needs a
File-System-Access-capable browser (Chrome/Edge); the picker must be triggered by the
`<CR>` keystroke (a user gesture).

`build.sh` also copies the web-tree-sitter highlighter assets into `web/vendor/`
(generating them once in the local `treesitter/` tooling dir, which owns the pinned
grammar deps). Highlighting is optional: skip it and the renderer falls back to plain text.

## Serving in production

The editor is **static files** — serve `web/` (this dir) with the built `dist/` (`eh.mjs`
+ `eh.wasm`) alongside it; no server-side code runs. The one hard requirement is
**cross-origin isolation**: the Worker's run loop parks on a `SharedArrayBuffer` (slice
5d), which the browser only grants when the document is cross-origin isolated. So every
response must carry:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Resource-Policy: same-origin   # so same-origin subresources satisfy COEP
```

Without them the page still runs but degrades to the slower `postMessage` transport and
**timers never fire** (`window.__bemtvi.sab` reports which mode is active). The `.wasm`
must also be served as `application/wasm` (most hosts do this by extension).

- **Netlify (two sites):** the **standard editor** is wired up — `../../netlify.toml` (base
  directory = repo root) runs `netlify-build.sh` (provisions Rust + the emscripten `emcc` linker
  via the shared `netlify-provision.sh`, runs `build.sh`, then assembles a clean static root at
  `_site/`: `web/` + `dist/` as siblings with `_headers` + `_redirects` at the root). The
  **python demo** is a *separate* Netlify site built by `netlify-build-demo.sh` (same toolchain →
  `build.sh` → `package-site.sh --demo` → `_site-demo/`, with Pyodide + `localHost:true`).
  Netlify resolves a `netlify.toml` relative to each site's **base directory** and file-based
  `[build]` settings can't be overridden in the dashboard, so the demo can't share the root
  config with blank UI build settings — the root `[build]` would win. Instead it has its own
  `crates/bemtvi-edithost/netlify.toml`; in the dashboard set the demo site's **Base directory** to
  `crates/bemtvi-edithost` (build command + publish + env come from that file — leave the UI build
  fields blank). Each `_site*` root carries its own `_headers` + `_redirects`, so that
  `netlify.toml` needs no `[[redirects]]`.
- **Cloudflare Pages / any `_headers` host:** the `web/_headers` file already sets all
  three for `/*`. Publish a root with `web/` and `dist/` as siblings, `_headers` at the
  root, and `/` → `/web/` (the layout `netlify-build.sh` assembles in `_site/`).
- **nginx:**
  ```nginx
  location / {
    add_header Cross-Origin-Opener-Policy   "same-origin"  always;
    add_header Cross-Origin-Embedder-Policy "require-corp" always;
    add_header Cross-Origin-Resource-Policy "same-origin"  always;
    types { application/wasm wasm; }
  }
  ```
- **Apache (`.htaccess`):**
  ```apache
  Header always set Cross-Origin-Opener-Policy   "same-origin"
  Header always set Cross-Origin-Embedder-Policy "require-corp"
  Header always set Cross-Origin-Resource-Policy "same-origin"
  AddType application/wasm .wasm
  ```
- **Any host:** `web/serve.mjs` is a reference Node static server that sets exactly these
  (it's what the dev workflow and `verify.mjs` use); copy its header logic if you roll
  your own.
