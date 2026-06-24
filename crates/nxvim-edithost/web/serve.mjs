// A tiny static dev server for the wasm edit-host (Phase 5). It serves this crate's
// directory and — crucially — sends the cross-origin-isolation headers a
// SharedArrayBuffer needs (slice 5d's input/timer park): COOP `same-origin` +
// COEP `require-corp`, plus CORP `same-origin` on every response so same-origin
// subresources (the worker, eh.mjs, eh.wasm) satisfy COEP. With these,
// `crossOriginIsolated === true` in the page. (Slice 5e ships the production serving
// docs; this is the dev/CI server the Playwright verifier — verify.mjs — runs against.)
//
//   node serve.mjs [port]      # default 8088; open http://localhost:<port>/web/
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// The static root to serve. Defaults to the crate dir (the standard editor: web/ + dist/).
// `NXVIM_SERVE_ROOT` points it at an assembled site instead — e.g. the python-demo's
// `demo-site/` (build-demo.sh), which has the same web/ + dist/ layout.
const ROOT = process.env.NXVIM_SERVE_ROOT
  ? resolve(process.env.NXVIM_SERVE_ROOT)
  : fileURLToPath(new URL("..", import.meta.url));
const PORT = Number(process.argv[2]) || 8088;

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".css": "text/css; charset=utf-8",
};

const server = createServer(async (req, res) => {
  // Cross-origin isolation (SAB prerequisite) on every response.
  res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
  res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
  res.setHeader("Cross-Origin-Resource-Policy", "same-origin");

  const urlPath = decodeURIComponent(new URL(req.url, "http://x").pathname);
  // The app lives under /web/; redirect the bare root there rather than serving
  // index.html *at* `/` — the page loads `./worker.mjs` / `../dist/eh.mjs` relative to
  // the document URL, so it only resolves correctly when that URL is under /web/.
  if (urlPath === "/") {
    res.writeHead(302, { Location: "/web/" }).end();
    return;
  }
  // Resolve under ROOT and refuse any traversal escape.
  const filePath = normalize(join(ROOT, urlPath));
  if (!filePath.startsWith(ROOT)) {
    res.writeHead(403).end("forbidden");
    return;
  }
  try {
    // A directory request (e.g. /web/) serves its index.html, so the page's relative
    // subresources resolve under that directory.
    let target = filePath;
    if ((await stat(target)).isDirectory()) target = join(target, "index.html");
    const body = await readFile(target);
    res.writeHead(200, { "Content-Type": MIME[extname(target)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

server.listen(PORT, () => {
  console.log(`serving ${ROOT} at http://localhost:${PORT}/web/  (cross-origin isolated)`);
});
