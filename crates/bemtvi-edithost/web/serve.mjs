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
// `BEMTVI_SERVE_ROOT` points it at an assembled site instead — e.g. the python-demo's
// `demo-site/` (build-demo.sh), which has the same web/ + dist/ layout.
const ROOT = process.env.BEMTVI_SERVE_ROOT
  ? resolve(process.env.BEMTVI_SERVE_ROOT)
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
  // A small same-origin test API for the `btv.http` verifier (verify-http.mjs): fixed
  // responses the serverless browser `fetch()` can hit without CORS. Additive — no other
  // harness touches /api/.
  if (urlPath.startsWith("/api/")) {
    const send = (code, type, body) => {
      res.writeHead(code, { "Content-Type": type });
      res.end(body);
    };
    switch (urlPath) {
      case "/api/hello":
        return send(200, "text/plain", "hello world");
      case "/api/data":
        return send(200, "application/json", JSON.stringify({ name: "btv", count: 3 }));
      case "/api/missing":
        return send(404, "text/plain", "nope");
      case "/api/target":
        // Echo the full request target (path + query) so a test can read the query
        // string the client built + encoded.
        return send(200, "text/plain", req.url);
      case "/api/echo": {
        const chunks = [];
        req.on("data", (c) => chunks.push(c));
        req.on("end", () => send(200, "text/plain", Buffer.concat(chunks).toString("utf8")));
        return;
      }
      default:
        return send(404, "text/plain", "unknown api route");
    }
  }
  // The app lives under /web/; redirect the bare root there rather than serving
  // The `btv.http.mount` Service Worker. Two special cases, both about SCOPE:
  //
  //  * it is served from the ROOT path even though the source lives in web/, because a SW's
  //    default scope is its own directory — from /web/ it could not see /plugin/* at all;
  //  * `Service-Worker-Allowed: /` is what lets it *register* with scope "/" while being
  //    fetched from this path. Without the header the browser rejects the registration.
  //
  // Netlify's `_headers` / netlify.toml carry the same header for the deployed site.
  if (urlPath === "/btv-sw.js") {
    try {
      const body = await readFile(join(ROOT, "web/btv-sw.js"));
      res.writeHead(200, {
        "Content-Type": "text/javascript",
        "Service-Worker-Allowed": "/",
        // A stale SW would serve a mount contract the editor no longer speaks.
        "Cache-Control": "no-cache",
      });
      res.end(body);
    } catch {
      res.writeHead(404).end("not found");
    }
    return;
  }
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
