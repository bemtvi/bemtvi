// Playwright verifier for `nx.http.fetch` in the SERVERLESS browser edit-host — the leg
// that runs the round-trip through the browser's own `fetch()` (no daemon). The editor
// enqueues each request off-tick (eh_take_http_requests); the Worker runs `fetch()` and
// lands the `["ok"|"err", …]` envelope back via eh_http_result, resolving the Lua promise.
// This drives it through a real (headless Chromium) browser against a same-origin test API
// served by serve.mjs (/api/*), so no CORS is involved.
//
// The daemon `http_op` leg (a native-daemon / browser-with-daemon session) shares the same
// wire + codec and is covered natively by crates/nxvim-server/tests/daemon_http.rs.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-http.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8121;
const ORIGIN = `http://localhost:${PORT}`;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const pats = [
    `${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`,
    `${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/*.app/Contents/MacOS/*`,
  ];
  for (const p of pats) {
    const found = globSync(p).sort();
    if (found.length) return found[found.length - 1];
  }
  return undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

async function waitReady(page) {
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
}

// Run `code` in the editor's Lua, returning its `return` value.
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// `nx.http.fetch` settles on a later tick — poll `_G.<key>` (rendered by rmpv as an
// `ok:…` / `err:…` string) until it's no longer nil (`ok:Nil`), or give up. A no-op feed
// each iteration advances the tick so the Worker drains the http reply into Lua.
async function pollGlobal(page, key, tries = 120) {
  for (let i = 0; i < tries; i++) {
    const v = String(await luaResult(page, `return _G.${key}`));
    if (!v.includes("Nil")) return v;
    await page.evaluate(() => window.__nxvim.feed("<Esc>"));
    await sleep(30);
  }
  return "Nil";
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  // Sanity: the test API is up.
  const hello = await (await fetch(`${ORIGIN}/api/hello`)).text();
  check("test API /api/hello reachable", hello === "hello world", `got=${JSON.stringify(hello)}`);

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const context = await browser.newContext();
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/index.html`);
  await waitReady(page);

  // ── A 2xx GET resolves with status / ok / body / a header ──────────────────────────────
  await luaResult(
    page,
    `_G.g = nil
     nx.http.fetch('${ORIGIN}/api/hello'):next(function(r)
       _G.g = r.status .. '|' .. tostring(r.ok) .. '|' .. r:text() .. '|' .. (r.headers['content-type'] or '?')
     end)
     return 1`,
  );
  const got = await pollGlobal(page, "g");
  check(
    "serverless GET resolves 200 / ok / body / content-type header",
    got.includes("200|true|hello world|text/plain"),
    `got=${JSON.stringify(got)}`,
  );

  // ── res:json() decodes the body ────────────────────────────────────────────────────────
  await luaResult(
    page,
    `_G.j = nil
     nx.async(function()
       local r = nx.await(nx.http.fetch('${ORIGIN}/api/data'))
       local d = r:json()
       _G.j = d.name .. ':' .. tostring(d.count)
     end)()
     return 1`,
  );
  const json = await pollGlobal(page, "j");
  check("serverless res:json() decodes the body", json.includes("nx:3"), `got=${JSON.stringify(json)}`);

  // ── opts.query builds + encodes the query string (lib-backed, same in wasm) ─────────────
  await luaResult(
    page,
    `_G.q = nil
     nx.http.fetch('${ORIGIN}/api/target', { query = { q = 'hi there', n = 2 } })
       :next(function(r) _G.q = r:text() end)
     return 1`,
  );
  const target = await pollGlobal(page, "q");
  check(
    "serverless opts.query appends an encoded query string (q=hi+there)",
    target.includes("/api/target?") && target.includes("q=hi+there") && target.includes("n=2"),
    `got=${JSON.stringify(target)}`,
  );

  // ── A 404 RESOLVES (ok=false), it does not reject (fetch semantics) ────────────────────
  await luaResult(
    page,
    `_G.nf = nil
     nx.http.fetch('${ORIGIN}/api/missing')
       :next(function(r) _G.nf = 'resolved:' .. r.status .. ':' .. tostring(r.ok) end)
       :catch(function() _G.nf = 'rejected' end)
     return 1`,
  );
  const notFound = await pollGlobal(page, "nf");
  check(
    "serverless 404 resolves with ok=false (not a rejection)",
    notFound.includes("resolved:404:false"),
    `got=${JSON.stringify(notFound)}`,
  );

  // ── POST a JSON body → echoed back ─────────────────────────────────────────────────────
  await luaResult(
    page,
    `_G.e = nil
     nx.http.fetch('${ORIGIN}/api/echo', { method = 'POST', body = { hi = 'there' } })
       :next(function(r) _G.e = r:text() end)
     return 1`,
  );
  const echoed = await pollGlobal(page, "e");
  check(
    "serverless POST sends a JSON body (echoed verbatim)",
    echoed.includes("hi") && echoed.includes("there"),
    `got=${JSON.stringify(echoed)}`,
  );

  // ── nx.http.fetch_local runs on the browser fetch() (serverless: identical to fetch) ───
  await luaResult(
    page,
    `_G.loc = nil
     nx.http.fetch_local('${ORIGIN}/api/hello'):next(function(r) _G.loc = r:text() end)
     return 1`,
  );
  const loc = await pollGlobal(page, "loc");
  check(
    "serverless nx.http.fetch_local resolves via the browser fetch()",
    loc.includes("hello world"),
    `got=${JSON.stringify(loc)}`,
  );

  // ── A transport failure REJECTS with { message } ───────────────────────────────────────
  await luaResult(
    page,
    `_G.err = nil
     _G.ok = false
     nx.http.fetch('http://127.0.0.1:1/nope', { timeout = 1500 })
       :next(function() _G.ok = true end)
       :catch(function(e) _G.err = type(e.message) == 'string' and #e.message > 0 end)
     return 1`,
  );
  const rejected = await pollGlobal(page, "err");
  check(
    "serverless transport failure rejects with a { message } table",
    rejected.includes("true"),
    `got=${JSON.stringify(rejected)}`,
  );

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless nx.http.fetch works (browser fetch(): 2xx / json / 404-resolves / POST body / transport-reject)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
