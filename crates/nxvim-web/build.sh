#!/usr/bin/env bash
# Build the fully client-side WASM editor and emit the browser bundle into web/pkg/.
#
# Prerequisites (one-time):
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.123   # must match Cargo.toml
#
# Then serve the static web/ dir over http (a secure context — localhost counts —
# is required for the File System Access API), e.g.:
#   python3 -m http.server -d crates/nxvim-web/web 8000
set -euo pipefail
cd "$(dirname "$0")"

# 1. Tailwind → a static, minified web/tailwind.css (no runtime CDN). Needs Node;
#    `npm ci` if a lockfile exists, else `npm install` to fetch the pinned CLI.
#    (.npmrc sets ignore-scripts so the grammar packages don't run node-gyp.)
if [ -f package-lock.json ]; then npm ci; else npm install; fi
npm run build:css

# 1b. Tree-sitter assets → web/vendor/ (runtime + grammar .wasm + sanitized queries),
#     for the in-browser syntax highlighter. Regenerated from the pinned grammar
#     devDependencies, so web/vendor/ is gitignored like web/pkg and web/tailwind.css.
npm run build:treesitter

# 2. Compile the editor core to WebAssembly. nxvim-web is excluded from the
#    workspace, so cargo treats it as standalone and uses this crate's own target/.
cargo build --target wasm32-unknown-unknown --release

wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir web/pkg \
  --out-name nxvim_web \
  target/wasm32-unknown-unknown/release/nxvim_web.wasm

echo "built web/pkg/ — serve the web/ dir over http and open it in a browser:"
echo "  python3 -m http.server -d \"$(pwd)/web\" 8000"
