#!/usr/bin/env bash
# Build the wasm edit-host (Phase 5, slice 5b): compile the staticlib to
# wasm32-unknown-emscripten, then link it with emcc into an ES module (dist/eh.mjs +
# eh.wasm) that the node harness (harness.mjs) — and, later, the Web Worker (slice 5c) —
# drives. Unlike the throwaway demo this links the *real* EditHost tick (core + Lua +
# the full server glue).
#
# Prereqs: rustup target add wasm32-unknown-emscripten; emcc on PATH (an installed
# emsdk, or the Arch `emscripten` package at /usr/lib/emscripten).
set -euo pipefail
cd "$(dirname "$0")"

# emcc may live in an emsdk (sourced env) or the Arch system package. Use it if already
# on PATH, else try the emsdk env, else the Arch package dir.
if ! command -v emcc >/dev/null 2>&1; then
  if [ -f "$HOME/emsdk/emsdk_env.sh" ]; then
    # shellcheck disable=SC1091
    source "$HOME/emsdk/emsdk_env.sh" >/dev/null 2>&1
  elif [ -x /usr/lib/emscripten/emcc ]; then
    PATH="/usr/lib/emscripten:$PATH"
  fi
fi
command -v emcc >/dev/null 2>&1 || {
  echo "error: emcc not found — install emsdk or the system emscripten package first" >&2
  exit 1
}

# 1. Staticlib: Rust core + Lua + the server tick, plus the lua54/regex C, as wasm
#    objects. This crate's nxvim-server dep is `default-features = false`, so the
#    `native` feature and its non-emscripten deps drop out; the Lua backend (PUC
#    lua54, the only backend) comes from the shared mlua dep. -fwasm-exceptions
#    aligns Rust's wasm EH with the vendored C's (puc-lua-compiles-to-wasm-emscripten).
EMCC_CFLAGS="-fwasm-exceptions" \
  cargo build --release --target wasm32-unknown-emscripten

LIB=target/wasm32-unknown-emscripten/release/libnxvim_edithost.a

# A Rust `staticlib` bundles Rust code but NOT the native C libraries its build scripts
# compiled (the vendored Lua, and nxvim-regex's C) — cargo records those as link
# directives we bypass by invoking emcc by hand. Locate and pass them explicitly; paths
# carry a build-hash, so find the newest match rather than pin it.
newest() { find target/wasm32-unknown-emscripten/release/build -path "$1" -print0 2>/dev/null \
  | xargs -0 ls -t 2>/dev/null | head -1; }
LUA_A=$(newest '*/out/lib/liblua5.*.a')
REGEX_A=$(newest '*/out/libnxvim_regex_c.a')
[ -n "$LUA_A" ]   || { echo "error: liblua5.*.a not found (did the cargo build run?)" >&2; exit 1; }
[ -n "$REGEX_A" ] || { echo "error: libnxvim_regex_c.a not found" >&2; exit 1; }

# 2. Final link → an importable ES module. Archive order: the edit-host lib first, then
#    the C libs it depends on (wasm-ld pulls members to satisfy earlier undefineds).
#    --no-entry: this is a library, no main().
mkdir -p dist
emcc "$LIB" "$LUA_A" "$REGEX_A" -o dist/eh.mjs \
  -fwasm-exceptions \
  --no-entry \
  --js-library web/eh-lib.js \
  -sMODULARIZE=1 \
  -sEXPORT_ES6=1 \
  -sENVIRONMENT=node,web,worker \
  -sALLOW_MEMORY_GROWTH=1 \
  -sEXIT_RUNTIME=0 \
  -sSTACK_SIZE=8MB \
  -sEXPORTED_RUNTIME_METHODS=ccall,cwrap,UTF8ToString,HEAPU8 \
  -sEXPORTED_FUNCTIONS=_eh_new,_eh_input,_eh_input_mouse,_eh_source_lua,_eh_apply_remote_config,_eh_seed_remote_cwd,_eh_boot_finish,_eh_attach,_eh_set_clock,_eh_next_deadline,_eh_tick_timers,_eh_take_fs_requests,_eh_save_bytes,_eh_save_len,_eh_fs_read_complete,_eh_fs_write_complete,_eh_take_watch_requests,_eh_remote_file_changed,_eh_daemon_status,_eh_set_proc_host,_eh_take_proc_requests,_eh_proc_spawned,_eh_proc_stdout,_eh_proc_exited,_eh_take_fs_op_requests,_eh_fs_op_result,_eh_take_fs_watch_requests,_eh_fs_watch_change,_eh_fs_watch_err,_eh_take_terminal_requests,_eh_terminal_data,_eh_terminal_flush,_eh_terminal_exit,_eh_take_lsp_requests,_eh_lsp_stdout,_eh_lsp_stderr,_eh_lsp_exited,_eh_take_dproc_requests,_eh_dproc_out,_eh_dproc_exit,_eh_take_sock_requests,_eh_sock_connected,_eh_sock_data,_eh_sock_closed,_eh_take_clipboard_writes,_eh_clipboard_push,_eh_take_ts_requests,_eh_ts_install_complete,_eh_ts_seed_installed,_eh_export_shada,_eh_load_shada,_eh_exec_lua,_eh_redraw_json,_eh_lines,_eh_aux_lines,_eh_free_string,_eh_free,_malloc,_free

# 3. Tree-sitter highlighter assets → web/vendor/ (the web-tree-sitter runtime + the
#    per-language grammar .wasm + sanitized queries) for the in-page syntax highlighter
#    (web/highlight.js). The pinned grammar devDeps + generator live in the local
#    treesitter/ tooling dir (its own package.json + .npmrc, isolated from web/'s
#    Playwright install so the grammar packages' node-gyp scripts stay off); build them
#    there once (if absent) and copy them in, rather than duplicate ~13 MB of grammar
#    packages. treesitter/vendor/ and web/vendor/ are gitignored like dist/. The
#    highlighter is optional — index.html degrades to plain rendering if this is skipped.
TS="treesitter"
if [ ! -d "$TS/vendor" ]; then
  echo "generating tree-sitter assets in $TS (one-time)…"
  ( cd "$TS" && { [ -f package-lock.json ] && npm ci || npm install; } && npm run build:treesitter ) \
    || echo "warn: tree-sitter asset generation failed — highlighting will be off (plain rendering)"
fi
if [ -d "$TS/vendor" ]; then
  mkdir -p web/vendor
  cp -r "$TS/vendor/." web/vendor/
  echo "copied tree-sitter assets → web/vendor/"
fi

# 4. msgpack ESM → web/vendor/msgpack/ for the WebTransport RPC client (web/rpc.mjs).
#    Required, not optional: worker.mjs statically imports rpc.mjs, which statically
#    imports vendor/msgpack — so the Worker fails to load without it even in serverless
#    OPFS mode. Vendored from the web/ devDependency rather than committed (web/vendor is
#    gitignored, regenerated like the tree-sitter assets). Self-heal like the tree-sitter
#    step: populate web/node_modules when absent (e.g. on Netlify, where nothing runs the
#    web install). Skip Playwright's browser binaries — the other web devDep is the test
#    harness, irrelevant to the build. Multi-file ESM (a utils/ subdir): copy the whole
#    dist.esm tree, then drop the .d.ts/.map/tsbuildinfo the browser never imports.
MSGPACK_SRC="web/node_modules/@msgpack/msgpack/dist.esm"
if [ ! -d "$MSGPACK_SRC" ]; then
  echo "installing web deps for @msgpack/msgpack (one-time)…"
  ( cd web && export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
    { [ -f package-lock.json ] && npm ci || npm install; } )
fi
[ -d "$MSGPACK_SRC" ] || {
  echo "error: $MSGPACK_SRC missing after web install — the Worker can't load without it" >&2
  exit 1
}
rm -rf web/vendor/msgpack && mkdir -p web/vendor/msgpack
cp -r "$MSGPACK_SRC/." web/vendor/msgpack/
find web/vendor/msgpack \( -name '*.d.ts' -o -name '*.map' -o -name '*.tsbuildinfo' \) -delete
echo "copied @msgpack/msgpack → web/vendor/msgpack/"

# This is the **standard editor** build: no Pyodide, and `build-config.js` stays `localHost:
# false`, so the Worker never installs the local process host. The python demo is a *separate*
# build — `build-demo.sh` — which assembles its own self-contained site (Pyodide + the local
# host) into demo-site/. Drop any Pyodide a previous demo build may have left in the shared
# vendor dir, so the standard site is clean.
rm -rf web/vendor/pyodide

echo
echo "built dist/eh.mjs (standard editor) — run the harness:  node harness.mjs"
echo "for the python demo site, run:  ./build-demo.sh"
