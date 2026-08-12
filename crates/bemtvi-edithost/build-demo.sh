#!/usr/bin/env bash
# Build the **python-demo** site — a separate, self-contained static site from the standard
# editor (build.sh). It is the standard build PLUS the in-browser python toolchain:
#   - Pyodide (CPython → wasm) vendored in, so `:terminal python <file>` runs locally; and
#   - build-config.js flipped to `localHost: true`, so the Worker installs the local process
#     host (web/local-host.mjs + web/pyodide-worker.mjs).
# The output is assembled under demo-site/ (gitignored) as `web/` + `dist/` siblings — the same
# curated publish layout package-site.sh produces for deploys — so it deploys (and serves)
# independently of the standard editor. The standard build (build.sh / web/) is untouched: its
# build-config.js stays `localHost: false` and it ships no Pyodide.
# See docs/plans/2026-06-23-web-python-demo.md.
#
# Run:  ./build-demo.sh   then   BEMTVI_SERVE_ROOT=demo-site node web/serve.mjs
set -euo pipefail
cd "$(dirname "$0")"

# 1. The standard build first: wasm → dist/, msgpack + tree-sitter → web/vendor/ (and it npm-
#    installs the web deps, incl. Pyodide, into web/node_modules). Everything shared comes here.
./build.sh

# 2. Assemble the curated python-demo site (build-config localHost:true + the demo-only modules
#    + Pyodide vendored in) — the same packager the Netlify demo deploy uses.
./package-site.sh demo-site --demo

echo
echo "assembled python-demo site → demo-site/ (web/ + dist/)"
echo "serve it:  BEMTVI_SERVE_ROOT=demo-site node web/serve.mjs   then open http://localhost:8088/web/"
echo "verify it: node web/verify-pyodide-terminal.mjs"
