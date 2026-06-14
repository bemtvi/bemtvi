# nxvim-edithost — the wasm edit-host

The real editor in the browser: the reusable synchronous [`EditHost`] tick (nxvim's
core + the PUC Lua 5.1 VM + the `vim.*` bindings + the full server glue — autocmds,
mirrors, lifecycle, the redraw projection) compiled to `wasm32-unknown-emscripten`,
driven behind a wasm [`HostEffects`] (`WasmEffects`) inside a Web Worker. This is Phase 5
of
[`docs/plans/2026-06-09-edit-host-and-browser-lua.md`](../../docs/plans/2026-06-09-edit-host-and-browser-lua.md).

It links the **production** `nxvim-server` tick (the keystroke path is the same one the
native server drives — not a hand-wired minimal tie-in).

## What runs here vs. not (serverless, or a daemon over WebTransport)

The browser build runs **serverless by default**: no daemon, so anything that needs a
*process* is unavailable and fails *loud*, never silently. Files, however, persist —
they live in the browser's Origin Private File System (OPFS, Phase 6a).

**Optional daemon mode (Phase 6b):** opening the page with
`?daemon=nxvim://HOST:PORT/TOKEN?cert=HASH` (the connect string a `nxvim --daemon --listen`
prints) makes `:e` / `:w` / `:e <dir>` operate on the **daemon's** filesystem over a real
**WebTransport (HTTP/3 / QUIC)** connection instead of OPFS — the browser twin of the native
daemon fs leg, editing still entirely in the Worker (only fs crosses the wire). The off-tick
seam is identical; only the transport differs (OPFS ↔ the `web/rpc.mjs` msgpack-RPC client
over a bidi stream). Config + shada stay **local** (OPFS) even in daemon mode. The other
daemon legs (processes / LSP / watch / `sys_run` / `luafs`) over WebTransport are later
slices — so a daemon session still fails `system()` loud for now.

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
- ❌ Processes — `vim.fn.system` / `nx._system` fail loud with a named "not available in
  the browser build yet" (`WasmBlockingSystem`); the async spawn path (`LoopOp::Spawn`)
  likewise echoes loud. The Phase 6 daemon over WebTransport will re-enable them (the fs
  leg landed in 6b; the process/LSP/watch legs are later slices).
- ❌ LSP and **native** treesitter — gated off the build (slice 5a); `:TSInstall` echoes
  a loud "not available in the browser build yet". Syntax **highlighting** is still
  present: done JS-side in the UI thread via web-tree-sitter (`web/highlight.js` + the
  `web/vendor/` grammars), exactly as `nxvim-web` does — the Worker ships the focused
  buffer's text with each frame and the page parses + colors it.

## Interop

emscripten `ccall`/`cwrap` over the `#[no_mangle] extern "C"` exports in `src/lib.rs`
(`eh_new` / `eh_input` / `eh_input_mouse` / `eh_source_lua` / `eh_boot_finish` /
`eh_attach` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` / the OPFS fs leg
`eh_take_fs_requests` / `eh_save_bytes` / `eh_save_len` / `eh_fs_read_complete` /
`eh_fs_write_complete` / `eh_free*`) — **not** wasm-bindgen
(that's `nxvim-web`'s `unknown-unknown` build). The
redraw comes back as a JSON return value. Slice 5c runs these exports **inside a Web
Worker** and ferries the JSON redraw to the UI over `postMessage`. Slice 5d drives the
Worker's run loop off a `SharedArrayBuffer` + `Atomics.wait` park: the same wait that
blocks on input also fires Worker-side timers (`vim.defer_fn` / `nx.timer`) via
`eh_set_clock` / `eh_next_deadline` / `eh_tick_timers` — one mechanism, no Asyncify.

## The browser shell — `web/`

- `web/worker.mjs` — the Web Worker, the single `!Send` thread that owns core + Lua. It
  loads `dist/eh.mjs`, constructs the real `EditHost`, and drives the production tick.
  When the page is cross-origin isolated (5d) it runs a blocking loop parked on
  `Atomics.wait` against an SAB input ring, waking on a keystroke **or** the next timer
  deadline; otherwise (5c) it is `postMessage`-driven (input works, timers don't fire).
  The latest `redraw` frame + buffer lines post back to the UI either way.
- `web/index.html` — the UI thread: the **DOM renderer** (the same per-cell-span renderer
  `nxvim-web` uses — windows/gutter/status/tabline/panel/pmenu, selection + cursor-shape
  classes, smooth scroll), driven off the server `redraw` frame; it translates keystrokes
  to vim key-notation and mouse gestures to `eh_input_mouse`, picks the SAB or postMessage
  transport by capability, and exposes `window.__nxvim` (`feed` / `mouse` / `execLua` /
  `attach` / `lines` / `frame` / `cursor` / `cmdline` / `sab` / …) for automation.
- `web/highlight.js` — the client-side web-tree-sitter highlighter (copied from
  `nxvim-web`); its grammars/runtime are generated into `web/vendor/` by `build.sh`
  (gitignored). The import is optional — the renderer degrades to plain text if absent.
- `web/serve.mjs` — a cross-origin-isolated dev server (COOP/COEP/CORP), so the page can
  use a `SharedArrayBuffer`. `crossOriginIsolated === true`.
- `web/rpc.mjs` — the browser↔daemon msgpack-RPC client (Phase 6b): the JS twin of
  `nxvim-rpc`, over one WebTransport bidi stream. `dialDaemon(uri)` connects (token on the
  CONNECT path, cert hash pinned TOFU); the Worker uses it to fulfill `:e`/`:w` over the wire
  in daemon mode. msgpack is the vendored `@msgpack/msgpack` (`web/vendor/msgpack/`, staged by
  `build.sh`); its `decodeMultiStream` frames the bidi stream.
- `web/verify.mjs` — the Playwright verifier: drives the real wasm edit-host in a real
  headless Chromium through `window.__nxvim` and asserts buffer / cursor / redraw, **and**
  that a deferred timer fires unattended via the SAB park (plus the OPFS round-trip).
- `web/verify-ui.mjs` — the renderer/input verifier: asserts the DOM renderer (not a
  `<pre>`), the command-line-below-status-line layout, visual-mode selection painting,
  cursor-shape-by-mode, mouse click / drag-select / wheel-scroll, and tree-sitter
  highlight colors — all in a real headless browser.
- `web/verify-config.mjs` — the config verifier: writes an `/init.lua` to OPFS, reloads,
  and asserts the config applied on startup (an option, a global, a keymap that fires,
  and a startup-buffer `BufEnter` autocmd) and that a broken config is non-fatal.
- `web/verify-daemon.mjs` — the daemon verifier (Phase 6b): spawns a real
  `nxvim --daemon --listen`, opens the page with `?daemon=<uri>`, and asserts `:e`/`:w`/`:e
  <dir>` round-trip over WebTransport — the saved bytes read back from the daemon's disk in
  Node (so they truly crossed the wire), `[+]` clearing only on the daemon's ack. The browser
  twin of native `daemon_quic.rs`. (Needs `cargo build -p nxvim` for the daemon binary.)

> **Note (not a gap, and not wasm-specific):** the Lua `vim.api.nvim_buf_*` *mutation*
> surface (`nvim_buf_set_lines` / `set_text` / `set_name`, `nvim_open_win`,
> `nvim_create_buf`, `nvim_feedkeys`, …) reads as `nil` here — but **by design, in every
> build**: nxvim's config API is autocmds / diagnostics / keymaps / options, not entity
> mutation (see `crates/nxvim-lua/src/prelude/api.lua`'s header). The *read* getters
> (`nvim_buf_get_lines`, …) and extmarks do exist. Mutate a buffer via keystrokes /
> ex-commands / `vim.cmd`.

## Build & run

```sh
rustup target add wasm32-unknown-emscripten   # once
# plus emcc: an installed+activated emsdk, or the system emscripten package
./build.sh         # → dist/eh.mjs + eh.wasm
node harness.mjs   # node smoke test: feeds `ihello<Esc>`, asserts lines + a real redraw

# the browser shell:
cd web && npm install        # playwright (once); plus `npx playwright install chromium`
node serve.mjs               # open http://localhost:8088/web/  to use the editor
node verify.mjs              # headless-browser proof of the editor/transport/OPFS contract
node verify-ui.mjs           # headless-browser proof of the renderer/mouse/selection/highlighting
node verify-config.mjs       # headless-browser proof of single-file init.lua sourcing from OPFS
node verify-daemon.mjs       # headless-browser proof of :e/:w/:e<dir> over WebTransport to a real --daemon

# daemon mode by hand: start a daemon, then open the page with its connect URI:
#   cargo run -p nxvim -- --daemon --listen 127.0.0.1:8765   # prints nxvim://…?cert=…
#   open  http://localhost:8088/web/?daemon=<that-nxvim://-uri>
```

**Configuring it:** drop a single self-contained `init.lua` at the OPFS root (`:w
/init.lua` from inside the editor, or write it via the File System API) — on the next
load the Worker sources it at startup. Options / keymaps / autocmds / user commands /
highlights apply; `require` of further modules / plugins does not (empty runtimepath).

`build.sh` also copies the web-tree-sitter highlighter assets into `web/vendor/`
(generating them once in the sibling `nxvim-web` crate, which owns the pinned grammar
deps). Highlighting is optional: skip it and the renderer falls back to plain text.

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
**timers never fire** (`window.__nxvim.sab` reports which mode is active). The `.wasm`
must also be served as `application/wasm` (most hosts do this by extension).

- **Netlify:** wired up — `../../netlify.toml` runs `netlify-build.sh` (provisions
  Rust + the emscripten `emcc` linker, runs `build.sh`, then assembles a clean static
  root at `_site/`: `web/` + `dist/` as siblings with `web/_headers` copied to the root
  so `/*` is cross-origin isolated) and redirects `/` → `/web/`. Connect the repo in the
  dashboard; every push to the production branch deploys.
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
