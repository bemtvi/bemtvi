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
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url)); // the crate dir
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

  let urlPath = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (urlPath === "/") urlPath = "/web/index.html";
  // Resolve under ROOT and refuse any traversal escape.
  const filePath = normalize(join(ROOT, urlPath));
  if (!filePath.startsWith(ROOT)) {
    res.writeHead(403).end("forbidden");
    return;
  }
  try {
    const info = await stat(filePath);
    if (info.isDirectory()) {
      res.writeHead(404).end("not found");
      return;
    }
    const body = await readFile(filePath);
    res.writeHead(200, { "Content-Type": MIME[extname(filePath)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});

server.listen(PORT, () => {
  console.log(`serving ${ROOT} at http://localhost:${PORT}/web/  (cross-origin isolated)`);
});
