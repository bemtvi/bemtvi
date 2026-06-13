# nxvim-edithost — the wasm edit-host

The real editor in the browser: the reusable synchronous [`EditHost`] tick (nxvim's
core + the PUC Lua 5.1 VM + the `vim.*` bindings + the full server glue — autocmds,
mirrors, lifecycle, the redraw projection) compiled to `wasm32-unknown-emscripten` and
driven behind a wasm [`HostEffects`] (`WasmEffects`). This is Phase 5, slice 5b of
[`docs/plans/2026-06-09-edit-host-and-browser-lua.md`](../../docs/plans/2026-06-09-edit-host-and-browser-lua.md).

It **supersedes** `nxvim-edithost-demo`. That throwaway proved only that core+Lua
*compile and run* together in wasm via a hand-wired minimal tie-in (no glue, no redraw);
this crate links the **production** `nxvim-server` tick. (The demo is deleted in slice
5e.)

## What runs here vs. not (serverless v1)

The browser build is **serverless**: there is no daemon, so anything that needs the
filesystem or a process is unavailable and fails *loud*, never silently:

- ✅ Core editing + the full Lua VM + autocmds + the redraw projection.
- ❌ Off-tick fs (open/save/watch over a daemon) — the `WasmEffects` fs methods are
  unreachable (`has_remote_fs() == false`) and `unreachable!`. The Phase 6 daemon over
  WebTransport re-enables remote files.
- ❌ LSP and native treesitter — gated off the build (slice 5a); `:TSInstall` echoes a
  loud "not available in the browser build yet". (Treesitter highlighting is done
  JS-side in `nxvim-web`.)

## Interop

emscripten `ccall`/`cwrap` over the `#[no_mangle] extern "C"` exports in `src/lib.rs`
(`eh_new` / `eh_input` / `eh_attach` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` /
`eh_free*`) — **not** wasm-bindgen (that's `nxvim-web`'s `unknown-unknown` build). The
redraw comes back as a JSON return value. Slice 5c runs these exports **inside a Web
Worker** and ferries the JSON redraw to the UI over `postMessage`; blocking input +
Worker-side timers over a `SharedArrayBuffer` are slice 5d.

## The browser shell (slice 5c) — `web/`

- `web/worker.mjs` — the Web Worker, the single `!Send` thread that owns core + Lua. It
  loads `dist/eh.mjs`, constructs the real `EditHost`, and drives the production tick;
  input arrives as `postMessage`s, the latest `redraw` frame + buffer lines post back.
- `web/index.html` — the UI thread: renders the server `redraw` frame into a character
  grid (mirroring the native TUI layout), translates keystrokes to vim key-notation, and
  exposes `window.__nxvim` (`feed` / `execLua` / `attach` / `lines` / `frame` / `cursor` /
  `cmdline` / …) for automation.
- `web/serve.mjs` — a cross-origin-isolated dev server (COOP/COEP/CORP), so the page can
  use a `SharedArrayBuffer` (slice 5d). `crossOriginIsolated === true`.
- `web/verify.mjs` — the Playwright verifier: drives the real wasm edit-host in a real
  headless Chromium through `window.__nxvim` and asserts buffer / cursor / redraw.

## Build & run

```sh
rustup target add wasm32-unknown-emscripten   # once
# plus emcc: an installed+activated emsdk, or the system emscripten package
./build.sh         # → dist/eh.mjs + eh.wasm
node harness.mjs   # node smoke test: feeds `ihello<Esc>`, asserts lines + a real redraw

# the browser shell:
cd web && npm install        # playwright (once); plus `npx playwright install chromium`
node serve.mjs               # open http://localhost:8088/web/  to use the editor
node verify.mjs              # headless-browser proof of slice 5c (PW_CHROMIUM overrides the binary)
```
