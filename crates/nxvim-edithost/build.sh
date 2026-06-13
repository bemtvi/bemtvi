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

# 1. Staticlib: Rust core + Lua + the server tick, plus the lua51/regex C, as wasm
#    objects. This crate's nxvim-server dep is `default-features = false`, so the
#    `native` feature and its non-emscripten deps drop out; the Lua backend (PUC
#    lua51, the only backend) comes from the shared mlua dep. -fwasm-exceptions
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
LUA_A=$(newest '*/out/lib/liblua5.1.a')
REGEX_A=$(newest '*/out/libnxvim_regex_c.a')
[ -n "$LUA_A" ]   || { echo "error: liblua5.1.a not found (did the cargo build run?)" >&2; exit 1; }
[ -n "$REGEX_A" ] || { echo "error: libnxvim_regex_c.a not found" >&2; exit 1; }

# 2. Final link → an importable ES module. Archive order: the edit-host lib first, then
#    the C libs it depends on (wasm-ld pulls members to satisfy earlier undefineds).
#    --no-entry: this is a library, no main().
mkdir -p dist
emcc "$LIB" "$LUA_A" "$REGEX_A" -o dist/eh.mjs \
  -fwasm-exceptions \
  --no-entry \
  -sMODULARIZE=1 \
  -sEXPORT_ES6=1 \
  -sENVIRONMENT=node,web,worker \
  -sALLOW_MEMORY_GROWTH=1 \
  -sEXIT_RUNTIME=0 \
  -sEXPORTED_RUNTIME_METHODS=ccall,cwrap,UTF8ToString,HEAPU8 \
  -sEXPORTED_FUNCTIONS=_eh_new,_eh_input,_eh_input_mouse,_eh_source_lua,_eh_boot_finish,_eh_attach,_eh_set_clock,_eh_next_deadline,_eh_tick_timers,_eh_take_fs_requests,_eh_save_bytes,_eh_save_len,_eh_fs_read_complete,_eh_fs_write_complete,_eh_export_shada,_eh_load_shada,_eh_exec_lua,_eh_redraw_json,_eh_lines,_eh_free_string,_eh_free,_malloc,_free

# 3. Tree-sitter highlighter assets → web/vendor/ (the web-tree-sitter runtime + the
#    per-language grammar .wasm + sanitized queries) for the in-page syntax highlighter
#    (web/highlight.js). The pinned grammar devDeps + generator live in the sibling
#    nxvim-web crate; build them there once (if absent) and copy them in, rather than
#    duplicate ~13 MB of grammar packages. web/vendor/ is gitignored like dist/. The
#    highlighter is optional — index.html degrades to plain rendering if this is skipped.
WEB="../nxvim-web"
if [ -d "$WEB" ]; then
  if [ ! -d "$WEB/web/vendor" ]; then
    echo "generating tree-sitter assets in $WEB (one-time)…"
    ( cd "$WEB" && { [ -f package-lock.json ] && npm ci || npm install; } && npm run build:treesitter ) \
      || echo "warn: tree-sitter asset generation failed — highlighting will be off (plain rendering)"
  fi
  if [ -d "$WEB/web/vendor" ]; then
    mkdir -p web/vendor
    cp -r "$WEB/web/vendor/." web/vendor/
    echo "copied tree-sitter assets → web/vendor/"
  fi
else
  echo "note: $WEB not found — skipping syntax-highlighter assets (plain rendering)"
fi

echo
echo "built dist/eh.mjs — run the harness:  node harness.mjs"
