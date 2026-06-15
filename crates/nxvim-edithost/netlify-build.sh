#!/usr/bin/env bash
# Netlify build entry for the nxvim wasm edit-host (crates/nxvim-edithost) — the real
# in-browser editor: nxvim-core + the PUC Lua 5.4 VM + the full server tick compiled to
# wasm32-unknown-emscripten and driven in a Web Worker.
#
# Netlify's Git integration runs this on every push to the production branch (see
# ../../netlify.toml); the published directory is the assembled `_site/` set there.
#
# This build links C (the vendored Lua + vim-regex), so it needs Emscripten's `emcc`
# final linker in addition to the Rust→wasm toolchain. We provision both, run the
# crate's build.sh, then assemble a clean static publish root.
#
# Node is provided by the image (NODE_VERSION in netlify.toml) for the tree-sitter
# vendoring step build.sh runs in the local treesitter/ tooling dir.
set -euo pipefail

# Pinned, overridable from netlify.toml's [build.environment].
TOOLCHAIN="${RUST_VERSION:-stable}"
EMSDK_VERSION="${EMSDK_VERSION:-6.0.0}"

here="$(cd "$(dirname "$0")" && pwd)"

# 1. Rust toolchain + the emscripten wasm target. Netlify's image ships `rustup` but
#    with no default toolchain configured, so we install & select it explicitly. No
#    wasm-bindgen here — this build links via emcc, not wasm-bindgen.
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
rustup toolchain install "$TOOLCHAIN" --profile minimal
rustup default "$TOOLCHAIN"
rustup target add wasm32-unknown-emscripten

# 2. Emscripten SDK (emcc). Install into $HOME/emsdk — the exact path build.sh probes
#    and sources when emcc isn't already on PATH. The toolchain is downloaded prebuilt
#    (a couple of minutes), not compiled; emsdk's dir is cacheable across builds.
if ! command -v emcc >/dev/null 2>&1; then
  if [ ! -d "$HOME/emsdk" ]; then
    git clone --depth 1 https://github.com/emscripten-core/emsdk.git "$HOME/emsdk"
  fi
  ( cd "$HOME/emsdk" && ./emsdk install "$EMSDK_VERSION" && ./emsdk activate "$EMSDK_VERSION" )
  # shellcheck disable=SC1091
  . "$HOME/emsdk/emsdk_env.sh"
fi

# 3. Build the wasm edit-host: cargo (wasm32-unknown-emscripten) → emcc link →
#    dist/eh.{mjs,wasm}, plus the web-tree-sitter highlighter assets copied into
#    web/vendor/ (generated once in the local treesitter/ tooling dir).
bash "$here/build.sh"

# 4. Assemble the static publish root (web/ + dist/ as siblings, _headers at the root,
#    the turnkey serve.mjs + a serving README) — the same layout the CI release tarball
#    ships, via the shared package-site.sh. netlify.toml redirects / → /web/.
bash "$here/package-site.sh" "$here/_site"
