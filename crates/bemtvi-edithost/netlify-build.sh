#!/usr/bin/env bash
# Netlify build entry for the **standard** bemtvi wasm edit-host (crates/bemtvi-edithost) — the
# real in-browser editor: bemtvi-core + the PUC Lua 5.4 VM + the full server tick compiled to
# wasm32-unknown-emscripten and driven in a Web Worker.
#
# Netlify's Git integration runs this on every push to the production branch (see
# ../../netlify.toml); the published directory is the assembled `_site/` set there.
#
# The python demo is a SEPARATE Netlify site built by netlify-build-demo.sh → _site-demo/ (see
# that file's header for the dashboard setup). This standard site ships no Pyodide.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

# 1+2. Provision the Rust→wasm toolchain + emcc (shared with the demo deploy).
# shellcheck disable=SC1091
. "$here/netlify-provision.sh"

# 3. Build the wasm edit-host: cargo (wasm32-unknown-emscripten) → emcc link →
#    dist/eh.{mjs,wasm}, plus the web-tree-sitter highlighter assets copied into
#    web/vendor/ (generated once in the local treesitter/ tooling dir).
bash "$here/build.sh"

# 4. Assemble the static publish root (web/ + dist/ as siblings, _headers + _redirects at the
#    root, the turnkey serve.mjs + a serving README) — the shared package-site.sh. Standard
#    flavor: build-config.js stays localHost:false, no Pyodide.
bash "$here/package-site.sh" "$here/_site"
