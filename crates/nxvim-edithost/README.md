# nxvim-edithost — the wasm edit-host

The real editor in the browser: the reusable synchronous [`EditHost`] tick (nxvim's
core + the PUC Lua 5.1 VM + the `vim.*` bindings + the full server glue — autocmds,
mirrors, lifecycle, the redraw projection) compiled to `wasm32-unknown-emscripten`,
driven behind a wasm [`HostEffects`] (`WasmEffects`) inside a Web Worker. This is Phase 5
of
[`docs/plans/2026-06-09-edit-host-and-browser-lua.md`](../../docs/plans/2026-06-09-edit-host-and-browser-lua.md).

It links the **production** `nxvim-server` tick (the keystroke path is the same one the
native server drives — not a hand-wired minimal tie-in).

## What runs here vs. not (serverless)

The browser build is **serverless**: there is no daemon, so anything that needs a
*process* is unavailable and fails *loud*, never silently. Files, however, persist —
they live in the browser's Origin Private File System (OPFS, Phase 6a):

- ✅ Core editing + the full Lua VM + autocmds + the redraw projection.
- ✅ **Files via OPFS** (Phase 6a): `:e` / `:w` open/save real files in the browser's
  Origin Private File System, so edits survive a reload. OPFS handle acquisition is
  *async* (only a `FileSystemSyncAccessHandle`'s operations are sync), so it can't back
  the *synchronous* core `HostFs` without Asyncify — instead the editor runs in **off-tick
  fs** mode (`has_remote_fs() == true`) and the Worker fulfills `:e`/`:w` against OPFS
  *between* ticks (`eh_take_fs_requests` → `eh_fs_read_complete` / `eh_fs_write_complete`),
  the same off-tick seam a daemon session uses, only the transport is OPFS. (The OPFS file
  *explorer* — `:e <dir>` — is a later slice.)
- ❌ Processes — `vim.fn.system` / `nx._system` fail loud with a named "not available in
  the browser build yet" (`WasmBlockingSystem`); the async spawn path (`LoopOp::Spawn`)
  likewise echoes loud. The Phase 6 daemon over WebTransport re-enables them.
- ❌ LSP and native treesitter — gated off the build (slice 5a); `:TSInstall` echoes a
  loud "not available in the browser build yet". (Treesitter highlighting is done
  JS-side in `nxvim-web`.)

## Interop

emscripten `ccall`/`cwrap` over the `#[no_mangle] extern "C"` exports in `src/lib.rs`
(`eh_new` / `eh_input` / `eh_attach` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` /
the OPFS fs leg `eh_take_fs_requests` / `eh_save_bytes` / `eh_save_len` /
`eh_fs_read_complete` / `eh_fs_write_complete` / `eh_free*`) — **not** wasm-bindgen
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
- `web/index.html` — the UI thread: renders the server `redraw` frame into a character
  grid (mirroring the native TUI layout), translates keystrokes to vim key-notation, picks
  the SAB or postMessage transport by capability, and exposes `window.__nxvim` (`feed` /
  `execLua` / `attach` / `lines` / `frame` / `cursor` / `cmdline` / `sab` / …) for automation.
- `web/serve.mjs` — a cross-origin-isolated dev server (COOP/COEP/CORP), so the page can
  use a `SharedArrayBuffer`. `crossOriginIsolated === true`.
- `web/verify.mjs` — the Playwright verifier: drives the real wasm edit-host in a real
  headless Chromium through `window.__nxvim` and asserts buffer / cursor / redraw, **and**
  that a deferred timer fires unattended via the SAB park.

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
node verify.mjs              # headless-browser proof of slices 5c+5d (PW_CHROMIUM overrides the binary)
```

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

- **Netlify / Cloudflare Pages:** the `web/_headers` file already sets all three for `/*`.
  Publish `web/` (with `dist/` copied in) — done.
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
