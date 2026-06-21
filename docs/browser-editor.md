# Browser editor

nxvim runs the **real editor — entirely in a browser tab, with no server**. Not a
cut-down demo or a syntax-highlighted textarea: `nxvim-core` **plus the PUC Lua 5.4
VM plus the production server tick** (autocmds, mirrors, the redraw projection — the
same keystroke path the native server drives) compile to WebAssembly and run
client-side. Your `init.lua` sources, your keymaps and autocmds fire, files open and
save — all in the tab.

Because the editing engine is local, typing has **zero round-trips**: motions,
operators, undo, and Lua all run in the page. Only the filesystem — and processes, in
daemon mode — ever crosses a wire, and even the filesystem can be the browser's own
storage. It's the [edit-host split](edit-host-split.md) taken to its limit: the local
half is a browser tab; the fs/process half is the browser's storage or a remote
daemon.

## What runs

- **The whole editor** — core + the full Lua VM + the server tick, in a Web Worker.
- **Your config** — `init.lua` is read from the browser's storage at boot and sourced
  through the real path, so options, keymaps, autocmds, user commands, and highlights
  all apply.
- **Files persist** — `:e` / `:w` open and save real files (see [Files](#files)),
  surviving a reload.
- **Syntax highlighting** — done JS-side via web-tree-sitter; `:TSInstall <lang>`
  fetches a prebuilt grammar on demand and caches it.

## What's different from native

| | In the browser |
| --- | --- |
| **Plugins** | `init.lua` is **one self-contained file** — a `require` of further modules / plugins doesn't resolve (the runtimepath is empty and storage reads are async). |
| **LSP** | Unavailable — fails loud, not silently. |
| **Native tree-sitter** | The in-process parser is gated off the build; highlighting uses the JS-side web-tree-sitter path instead. |
| **Processes** | Serverless: blocking `vim.fn.system` always fails loud, and async `vim.system` / `jobstart` fail loud too. Both run in **daemon mode** (see [Files](#files)). |
| **Lua dialect** | PUC Lua only — no LuaJIT `ffi` / `bit`. |
| **Hosting** | Requires **cross-origin isolation** (COOP/COEP) for the `SharedArrayBuffer`. Without it, input still works but timers (`nx.timer` / `vim.defer_fn`) don't fire. |

Anything unavailable **fails loud** with a named error rather than faking a result —
the same no-silent-stubs rule the rest of nxvim follows.

## Files

The browser is the filesystem, three ways — all riding the same off-tick fs seam the
native edit-host split uses, so only the transport differs:

- **OPFS (default, serverless).** `:e` / `:w` persist to the browser's Origin Private
  File System, and `:e <dir>` lists it. Your `init.lua` and shada live there too, so
  edits and config survive a reload.
- **Real local files.** The **File System Access API** backs `:eo` / `:wo` (and a bare
  `:w` on a bound path) — pick a real file or directory on disk through the browser's
  permission picker.
- **A real daemon.** Open the page with `?daemon=nxvim://HOST:PORT/TOKEN?cert=HASH`
  (the string `nxvim --daemon --listen` prints), or dial it at runtime with
  `:connect nxvim://…`, and `:e` / `:w` operate on the **daemon's** filesystem over
  **WebTransport** (HTTP/3 / QUIC). Editing still happens entirely in the tab — only
  fs crosses the wire — and config + shada stay local. Daemon mode also brings
  **async processes** (`vim.system` / `jobstart`) and **`:terminal`** over the wire;
  LSP over the wire is a later slice.

## Run it locally

```sh
cd crates/nxvim-edithost
./build.sh                  # cargo → emcc link → dist/eh.{mjs,wasm} + tree-sitter assets
cd web && npm install       # once: Playwright + chromium
node serve.mjs              # a cross-origin-isolated (COOP/COEP) dev server
# open http://localhost:8088/web/
```

`node harness.mjs` is a headless Node smoke test (feeds `ihello<Esc>`, asserts the
lines and a real redraw); the `verify-*.mjs` scripts drive the real wasm editor in a
headless browser (renderer, OPFS, the local-file picker, daemon mode, …).

## How it works (in brief)

- **A Web Worker owns the editor.** `web/worker.mjs` is the single `!Send` thread
  holding core + Lua; it loads the wasm module (`dist/eh.mjs`) and runs the production
  tick. The UI thread (`web/index.html`) paints the server `redraw` frame as
  **HTML/CSS** — a per-cell-span DOM renderer (windows, gutter, status/tabline, pmenu,
  cursor shapes, smooth scroll) — and translates `KeyboardEvent`s to vim key-notation.
  The two talk over `postMessage` and a shared ring.
- **One wait drives input *and* timers.** When cross-origin isolated, the Worker parks
  on `Atomics.wait` over a `SharedArrayBuffer` input ring, waking on a keystroke **or**
  the next timer deadline — so `nx.timer` / `vim.defer_fn` fire without Asyncify, one
  mechanism. (Without isolation it falls back to a `postMessage` loop where timers
  don't fire.)
- **Interop is emscripten `ccall`/`cwrap`, not wasm-bindgen** — it links the C lua54
  backend + vim-regex, so the final link is `emcc`. `src/lib.rs` exports `eh_input` /
  `eh_exec_lua` / `eh_redraw_json` / the fs legs / … and the redraw returns as JSON.
- **It's agent-drivable.** The page exposes a `window.__nxvim` hook (`feed` / `mouse` /
  `execLua` / `lines` / `frame`) so a headless browser (Playwright) can feed keys and
  assert on state — the same black-box style as the native test harness.
- **Built outside the workspace.** It targets `wasm32-unknown-emscripten` and links C
  via `emcc`, so it sits in the root `Cargo.toml`'s `[workspace] exclude` (a host
  `cargo build --workspace` never touches it) and pins its own deps. Deployed as static
  files (`netlify.toml`); the one hard requirement is cross-origin isolation.

## See also

- [The edit-host split](edit-host-split.md) — the same split reached natively, with
  the editor local and an `nxvim --daemon` serving fs / processes over ssh or QUIC.
- [Architecture → the web build](architecture.md#the-web-build--a-fully-client-side-webassembly-editor)
  — the crate layout and the full redraw/interop projection.
- [The edit-host & browser-Lua plan](plans/2026-06-09-edit-host-and-browser-lua.md) —
  the design and its phased implementation.
