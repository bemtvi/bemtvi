# Pickers + preview on remote / web

/ 2026-06-24

## The bug

When the editor is connected to a remote daemon, or running as the pure web
client (serverless wasm + OPFS, e.g. the python demo), three picker surfaces are
broken:

1. **File picker shows nothing.** `nx.picker` `files` shells out to `rg --files`
   (`picker.lua`). `rg` isn't present in the browser worker (the Pyodide proc
   host runs every spawn as `python …`, not a real binary) and isn't guaranteed
   on a remote daemon. The spawn fails / returns nothing and the picker is
   silently empty.
2. **Grep picker shows nothing.** Same cause — `live_grep` shells out to
   `rg --vimgrep`.
3. **Buffer-list preview is stuck.** The list itself works (in-memory buffer
   names), but the preview pane is permanently `"<path>: loading…"`:
   `read_preview_file` (`redraw.rs`) returns a placeholder whenever the FS is
   off-tick (daemon/wasm) and never fetches via the async seam.

A latent contributing bug: on the pure web client a spawn with no process host
(`effects.rs`, `LoopOp::Spawn`, `!has_remote_proc()`) only `echo`s and **drops
the callback**, leaving `nx.run` / `nx.run_stream` pending forever (a silent
hang, against the no-silent-stubs rule).

## Decisions

- **Prefer `rg`, then `grep`, fall back to an `nx.fs` walk/grep ONLY on the pure
  web client.** The transport-agnostic seam is `nx.fs` (readdir/read_text),
  which routes through the off-tick fs path that already works on native-local,
  native-daemon, web-daemon and web-OPFS alike (the same path `:e`/`:w` use).
  The native-only `host_fs_async`/tokio server seam is NOT usable for the web
  case, so the fix lives in Lua (`nx.fs`), not a server-side tokio fetch.
- **No fragile daemon-vs-Pyodide capability flag.** Make a hostless spawn
  complete LOUD with `code = -1` (a spawn failure, like a missing binary); then
  the picker simply falls back whenever `rg` yields nothing. This covers every
  mode (serverless → -1 → walk; Pyodide-runs-rg-as-python → no output → walk;
  native/daemon-with-rg → output → no walk) without distinguishing hosts.

## Phases

### Phase 1 — file picker (this pass)

- `effects.rs`: the wasm `LoopOp::Spawn` no-host branch delivers a `code = -1`
  process exit to the Lua callback (fail loud) instead of dropping it.
- `nx.fs.walk(dir[, opts])`: a recursive, transport-agnostic file enumeration
  (public, reused by the grep phase). Prunes `.git`/dotfiles by default, capped.
- `picker.lua` `files`: a fallback chain `rg --files` → `find` → `nx.fs.walk`. Each
  step runs only when the previous produced nothing; the binaries need a real shell,
  so the pure web client lands on the `nx.fs` walk.
- Tests: native integration test drives `nx.fs.walk` over a temp dir (hermetic,
  local LuaFs); a serverless-OPFS browser verify script proves the end-to-end
  picker fallback (run by hand — needs Chromium).

### Phase 2 — preview pane (this pass)

The reported preview failure is the **buffers** picker: its list works (in-memory
names) but the pane is stuck `"loading…"` off-tick. Those targets are *already-loaded
buffers*, so:

- `ensure_preview` (`redraw.rs`) prefers an already-loaded buffer's in-memory lines
  (`find_buffer_by_path` → `lines_of`) over a host-FS read. No filesystem, so it
  works off-tick (daemon / web) and reflects live unsaved edits. A file-picker item
  that happens to be open benefits too.

### Phase 2b — async preview of un-loaded files (done)

Previewing an **un-loaded** file off-tick (a fresh file-picker hit on a daemon / OPFS)
used to show `"loading…"` forever. `ensure_preview` now, on an off-tick miss, issues
an `fs_fetch` over the **same cross-transport seam `:edit` uses** — tagged with a
reserved buffer id `PREVIEW_FETCH_BUF` (`1 << 48`: far above any real bufnr, below
2^53 so it round-trips exactly through the wasm FFI's `f64` buffer id). The shared
open-landing (`apply_open` native / `complete_fs_read` wasm) branches on that id and
routes the bytes to `apply_preview`, which lands them into the existing `preview_cache`
+ repaints — never building a buffer (no lifecycle for a read-only preview). A landing
whose path no longer matches the cache is dropped (the selection moved on). No Worker
JS change: the sentinel id rides the existing fs request/echo verbatim.

Verified: a native daemon-wire test (`daemon_preview.rs`) previews a `/virtual` file the
edit-host's disk can't hold; a wasm node test (`verify-preview-fetch.mjs`) drives the
whole loop through the real tick and asserts the sentinel id round-trips + content lands.

### Phase 3 — grep without rg (done)

- `live_grep` falls back `rg --vimgrep` → `grep -rnI` → `nx.fs.grep` (a public,
  reusable walk + in-Lua **plain-substring** match). The binaries need a real shell,
  so the pure web client lands on `nx.fs.grep` over OPFS. Each step runs only when the
  previous found nothing. (Performance is a non-issue — the pure-client trees are small
  and the picker caps results.)
- `nx.fs.grep(dir, query[, opts])` added alongside `nx.fs.walk` (both reusable by
  plugin authors).
- **Preview path resolution** (a correctness fix the grep fallback surfaced): `rg`,
  `grep`, and `nx.fs` all emit cwd-RELATIVE paths, but the off-tick fs read carries no
  session cwd. `ensure_preview` now resolves a relative target against the effective
  cwd up front (window → tab → global, like `:edit`) so the cache key, the in-memory
  lookup, the sync read, and the async fetch/landing all agree on one absolute path —
  fixing relative-path previews on daemon/web for every source.

Tests: `nx.fs.grep` over a temp dir (hermetic, native LuaFs); a daemon-wire test that a
RELATIVE preview target resolves against the remote cwd; a browser serverless verify
script for the end-to-end grep fallback (run by hand — needs Chromium).
