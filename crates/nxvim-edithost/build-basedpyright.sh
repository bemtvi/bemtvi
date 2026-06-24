#!/usr/bin/env bash
# Build basedpyright's browser language-server worker FROM SOURCE and vendor it into
# web/vendor/basedpyright/pyright.worker.js (the python-demo LSP — Phase 4 of
# docs/plans/2026-06-23-web-python-demo.md).
#
# Why from source: the published `basedpyright` npm package ships only a closed webpack Node
# bundle (`dist/pyright-langserver.js`) that self-runs on stdio with no exports — unusable in a
# browser. basedpyright's monorepo, however, contains an OFFICIAL browser target
# (`packages/browser-pyright`, "browser-basedpyright") that builds with rspack to a self-contained
# Web Worker speaking LSP over `BrowserMessageReader/Writer`, with typeshed bundled in. We clone
# the repo at a pinned tag, build that package, and copy out the one worker file. web/local-host.mjs
# bridges the editor's stdio-framed JSON-RPC seam to/from that worker (uri rebasing under /w,
# workspaceFolders synthesis, pyright/createFile on didOpen — see that file).
#
# The clone + the vendored worker are gitignored (regenerated, not committed — the worker is ~16 MB).
# Idempotent: re-running skips the clone / npm install / build when their outputs already exist;
# pass --force to rebuild. Run standalone, or via build-demo.sh / package-site.sh --demo.
set -euo pipefail
cd "$(dirname "$0")"
here="$(pwd)"

PIN="v1.39.8"                          # basedpyright tag to build (keep in lock-step with the demo)
SRC="$here/.basedpyright-src"          # gitignored clone
OUT="$here/web/vendor/basedpyright"    # vendored worker destination (gitignored, copied by package-site)
WORKER="$SRC/packages/browser-pyright/dist/pyright.worker.js"

force=0
[ "${1:-}" = "--force" ] && force=1

if [ "$force" = 0 ] && [ -f "$OUT/pyright.worker.js" ]; then
  echo "basedpyright worker already vendored ($OUT/pyright.worker.js) — skip (pass --force to rebuild)"
  exit 0
fi

# 1. Clone at the pinned tag (shallow). Skipped if the tree is already present.
if [ ! -d "$SRC/.git" ]; then
  echo "cloning basedpyright $PIN → $SRC"
  rm -rf "$SRC"
  git clone --depth 1 --branch "$PIN" https://github.com/detachhead/basedpyright.git "$SRC"
fi

# 2. Install the monorepo's JS deps (npm workspaces). Skipped if node_modules exists.
if [ ! -d "$SRC/node_modules" ]; then
  echo "npm install (basedpyright monorepo) — this is slow the first time"
  ( cd "$SRC" && npm install )
fi

# 3. The rspack config bundles `<repo>/docstubs` (typeshed + injected CPython docstrings) into the
#    worker's virtual FS as the typeshed. Generating real docstubs needs extra Python tooling
#    (`docify`); we don't need the enhanced hover docstrings, so point `docstubs` at the plain
#    typeshed-fallback. Type-checking is identical — only docstring-rich hovers are slightly leaner.
if [ ! -e "$SRC/docstubs" ]; then
  ln -s "packages/pyright-internal/typeshed-fallback" "$SRC/docstubs"
fi

# 4. Build the browser worker (rspack, production).
echo "building browser-basedpyright (rspack)…"
( cd "$SRC/packages/browser-pyright" && npm run build )
[ -f "$WORKER" ] || { echo "error: build produced no $WORKER" >&2; exit 1; }

# 5. Vendor the single worker file (the .map is dev-only; skip it to keep the demo lean).
mkdir -p "$OUT"
cp "$WORKER" "$OUT/pyright.worker.js"
echo "vendored basedpyright worker → $OUT/pyright.worker.js ($(du -h "$OUT/pyright.worker.js" | cut -f1))"
