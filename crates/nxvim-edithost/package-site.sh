#!/usr/bin/env bash
# Assemble the static publish root for the wasm edit-host — the runtime files any static
# host serves: web/ + dist/ as siblings, the cross-origin-isolation _headers at the root,
# plus the reference node server (web/serve.mjs) and a short README so the packaged
# tarball is turnkey ("extract and serve"). Run build.sh first — it produces
# dist/eh.{mjs,wasm} and the web/vendor/ highlighter + msgpack assets this copies.
#
# Single source of truth for the publish layout, shared by:
#   - netlify-build.sh                       (the Netlify deploy)
#   - .github/workflows/build.yml `edithost` (the downloadable release tarball)
#
# Usage: package-site.sh [SITE_DIR]   (default: ./_site, wiped and rebuilt each run)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
site="${1:-$here/_site}"

[ -f "$here/dist/eh.mjs" ] && [ -f "$here/dist/eh.wasm" ] || {
  echo "error: dist/eh.{mjs,wasm} missing — run build.sh before package-site.sh" >&2
  exit 1
}

# The page's relative imports require web/ and dist/ as siblings (index.html loads
# ./worker.mjs; worker.mjs loads ../dist/eh.mjs). Copy only the runtime files — not the
# dev tooling, the Playwright verifiers, or package.json. serve.mjs *is* runtime here:
# it's the turnkey static server that sets the cross-origin-isolation headers (below).
rm -rf "$site"
mkdir -p "$site/web" "$site/dist"
cp "$here/dist/eh.mjs" "$here/dist/eh.wasm" "$site/dist/"
cp "$here/web/index.html" "$here/web/worker.mjs" "$here/web/highlight.js" \
   "$here/web/grammars.js" "$here/web/ts-sanitize.js" \
   "$here/web/serve.mjs" "$site/web/"
[ -d "$here/web/vendor" ] && cp -r "$here/web/vendor" "$site/web/vendor"

# Cross-origin-isolation headers for `_headers`-format hosts (Netlify / Cloudflare
# Pages): the Worker's run loop parks on a SharedArrayBuffer, granted only when the
# document is cross-origin isolated. Without these the page still runs but degrades to
# postMessage and timers never fire.
cp "$here/web/_headers" "$site/_headers"

# A short, self-contained serving guide at the tarball root — the one hard requirement
# (COOP/COEP) plus the turnkey command and the per-host header recipes.
cat > "$site/README.md" <<'EOF'
# nxvim edit-host — static site

The nxvim editor compiled to WebAssembly: a fully client-side editor, no server-side
code. These are plain static files — `web/` and `dist/` as siblings.

## Serve it

The one hard requirement is **cross-origin isolation**: the editor's run loop parks on a
`SharedArrayBuffer`, which the browser only grants when every response carries

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Resource-Policy: same-origin
```

Without them the page still loads but degrades to the slower postMessage transport and
**timers never fire**. `.wasm` must also be served as `application/wasm`.

**Turnkey (Node, no deps):** a reference server that sets exactly these headers ships in
the tarball —

```sh
node web/serve.mjs            # then open http://localhost:8088/web/
```

**Netlify / Cloudflare Pages / any `_headers` host:** the `_headers` file at this root
already sets all three for `/*`. Redirect `/` → `/web/`.

**nginx:**

```nginx
location / {
  add_header Cross-Origin-Opener-Policy   "same-origin"  always;
  add_header Cross-Origin-Embedder-Policy "require-corp" always;
  add_header Cross-Origin-Resource-Policy "same-origin"  always;
  types { application/wasm wasm; }
}
```

**Apache (`.htaccess`):**

```apache
Header always set Cross-Origin-Opener-Policy   "same-origin"
Header always set Cross-Origin-Embedder-Policy "require-corp"
Header always set Cross-Origin-Resource-Policy "same-origin"
AddType application/wasm .wasm
```

The app lives under `/web/` (its imports resolve relative to that document URL), so send
the bare root there: open `…/web/`, not `…/`.
EOF

echo "assembled static edit-host site → $site"
