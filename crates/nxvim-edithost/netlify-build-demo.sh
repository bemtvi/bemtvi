#!/usr/bin/env bash
# Netlify build entry for the **python-demo** site (crates/nxvim-edithost) — the standard wasm
# edit-host PLUS the in-browser python toolchain: Pyodide (CPython → wasm) vendored in and the
# local process host enabled (build-config localHost:true), so a serverless `:terminal python
# <file>` runs CPython in the browser with no backend.
#
# This is a SEPARATE Netlify site from the standard editor. The root netlify.toml configures the
# standard site (nxvim); configure the demo site (nxvim-demo) in the Netlify dashboard with:
#
#   Base directory:    (repo root — leave blank)
#   Build command:     bash crates/nxvim-edithost/netlify-build-demo.sh
#   Publish directory: crates/nxvim-edithost/_site-demo
#   Environment:       NODE_VERSION=24  RUST_VERSION=1.96.0  EMSDK_VERSION=6.0.0
#
# (Same toolchain env as the standard site; the script also defaults them if unset.) The
# assembled _site-demo/ carries its own _headers (cross-origin isolation — required for both
# the editor's SAB and Pyodide's interrupt) and _redirects (/ → /web/), so no per-site
# netlify.toml is needed. See docs/plans/2026-06-23-web-python-demo.md.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

# 1+2. Provision the Rust→wasm toolchain + emcc (shared with the standard deploy).
# shellcheck disable=SC1091
. "$here/netlify-provision.sh"

# 3. Build the wasm edit-host (same as the standard site; also npm-installs the web deps,
#    incl. Pyodide, into web/node_modules for the packager to vendor).
bash "$here/build.sh"

# 4. Assemble the python-demo publish root: build-config localHost:true + the demo-only modules
#    (local-host.mjs + pyodide-worker.mjs) + Pyodide vendored in. Same packager, --demo flavor.
bash "$here/package-site.sh" "$here/_site-demo" --demo
