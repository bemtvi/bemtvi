#!/usr/bin/env bash
# ⚠ TEMPORARY DEMO — see the banners in Cargo.toml / src/lib.rs. Delete with the crate.
#
# Builds the throwaway "does core+Lua run in wasm?" demo: compiles the staticlib to
# wasm32-unknown-emscripten, then links it with emcc into an ES module (dist/eh.mjs +
# eh.wasm) that the node harness (harness.mjs) drives.
#
# Prereqs: rustup target add wasm32-unknown-emscripten; an installed+activated emsdk.
set -euo pipefail
cd "$(dirname "$0")"

# emsdk provides emcc. Use it if already on PATH, else source the default location.
if ! command -v emcc >/dev/null 2>&1 && [ -f "$HOME/emsdk/emsdk_env.sh" ]; then
  # shellcheck disable=SC1091
  source "$HOME/emsdk/emsdk_env.sh" >/dev/null 2>&1
fi
command -v emcc >/dev/null 2>&1 || {
  echo "error: emcc not found — install and source emsdk first" >&2
  exit 1
}

# 1. Staticlib: Rust core + the lua51/regex C, as wasm objects. lua51 is mandatory
#    (LuaJIT is excluded from wasm); -fwasm-exceptions aligns Rust's wasm EH with the
#    vendored C's (project memory: wasm32-mlua / puc-lua-compiles-to-wasm-emscripten).
EMCC_CFLAGS="-fwasm-exceptions" \
  cargo build --release --target wasm32-unknown-emscripten

LIB=target/wasm32-unknown-emscripten/release/libnxvim_edithost_demo.a

# A Rust `staticlib` bundles Rust code but NOT the native C libraries its build
# scripts compiled (the vendored Lua, and nxvim-regex's C) — cargo records those as
# link directives we bypass by invoking emcc by hand. So locate and pass them
# explicitly. Paths carry a build-hash, so find the newest match rather than pin it.
newest() { find target/wasm32-unknown-emscripten/release/build -path "$1" -print0 2>/dev/null \
  | xargs -0 ls -t 2>/dev/null | head -1; }
LUA_A=$(newest '*/out/lib/liblua5.1.a')
REGEX_A=$(newest '*/out/libnxvim_regex_c.a')
[ -n "$LUA_A" ]   || { echo "error: liblua5.1.a not found (did the cargo build run?)" >&2; exit 1; }
[ -n "$REGEX_A" ] || { echo "error: libnxvim_regex_c.a not found" >&2; exit 1; }

# 2. Final link → an importable ES module. Archive order: the demo lib first, then
#    the C libs it depends on (wasm-ld pulls members to satisfy earlier undefineds).
#    --no-entry: this is a library, no main().
mkdir -p dist
emcc "$LIB" "$LUA_A" "$REGEX_A" -o dist/eh.mjs \
  -fwasm-exceptions \
  --no-entry \
  -sMODULARIZE=1 \
  -sEXPORT_ES6=1 \
  -sENVIRONMENT=node,web \
  -sALLOW_MEMORY_GROWTH=1 \
  -sEXIT_RUNTIME=0 \
  -sEXPORTED_RUNTIME_METHODS=ccall,cwrap,UTF8ToString \
  -sEXPORTED_FUNCTIONS=_eh_new,_eh_input,_eh_exec_lua,_eh_lines,_eh_free_string,_eh_free,_malloc,_free

echo
echo "built dist/eh.mjs — run the demo:  node harness.mjs"
