// verify-http-mount.mjs — the serverless-browser `btv.http.mount` leg, end to end.
//
// A tab cannot bind a TCP port, so the web build serves a plugin's mounted subroutes through
// a Service Worker (web/btv-sw.js) that intercepts `/plugin/*` on the page's own origin and
// relays each request to the edit-host. This drives that whole chain in a real headless
// Chromium and — the point — fetches the mount as an ORDINARY URL:
//
//   page fetch("/plugin/x/") → SW → window → ring frame → wasm → Lua on_request → respond
//                            → window → SW port → the fetch resolves
//
// A pass means every hop worked; nothing here is mocked. Needs `node build.sh` to have run
// (the wasm carries the eh_http_* mount exports) and a Chromium for Playwright (PW_CHROMIUM
// on macOS). Run: node verify-http-mount.mjs

import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const PORT = 8127;
const ORIGIN = `http://localhost:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Playwright's bundled Chromium (PW_CHROMIUM overrides; else newest cached build).
function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const roots = [
    join(homedir(), "Library/Caches/ms-playwright"),
    join(homedir(), ".cache/ms-playwright"),
  ];
  for (const root of roots) {
    if (!existsSync(root)) continue;
    const builds = readdirSync(root)
      .filter((d) => d.startsWith("chromium-"))
      .sort((a, b) => Number(b.split("-")[1]) - Number(a.split("-")[1]));
    for (const b of builds) {
      for (const rel of [
        "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        // Playwright renamed the Linux payload dir `chrome-linux` → `chrome-linux64`
        // (and ships `chrome-linux-arm64`); try each, newest layout first.
        "chrome-linux64/chrome",
        "chrome-linux-arm64/chrome",
        "chrome-linux/chrome",
      ]) {
        const p = join(root, b, rel);
        if (existsSync(p)) return p;
      }
    }
  }
  return undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    failures++;
    if (detail !== undefined) console.log(`      ${detail}`);
  }
}

const server = spawn(process.execPath, [new URL("./serve.mjs", import.meta.url).pathname, String(PORT)], {
  stdio: "inherit",
});
const cleanup = () => server.kill();
process.on("exit", cleanup);

const waitReady = async (page) => {
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
};
const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

// Poll `_G.<key>` (rendered by rmpv as an `ok:…` / `err:…` string) until it is no longer nil
// (`ok:Nil`). The mount settles on a LATER tick — Service Worker registration plus the ring
// round-trip — and each iteration feeds `<Esc>` so the Worker's loop actually advances.
async function pollGlobal(page, key, tries = 120) {
  for (let i = 0; i < tries; i++) {
    const v = String(await luaResult(page, `return _G.${key}`));
    if (!v.includes("Nil")) return v;
    await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
    await sleep(50);
  }
  return "Nil";
}

try {
  // Wait for the dev server.
  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`${ORIGIN}/web/index.html`);
      break;
    } catch {
      await sleep(100);
    }
  }

  // The SW must be served from the ROOT with `Service-Worker-Allowed: /`, or a scope-"/"
  // registration is rejected outright and every mount 404s. Assert the wiring before the
  // browser, so a header regression reads as itself rather than as a mysterious timeout.
  const swRes = await fetch(`${ORIGIN}/btv-sw.js`);
  check("btv-sw.js is served from the root", swRes.status === 200, `status ${swRes.status}`);
  check(
    "btv-sw.js carries Service-Worker-Allowed: /",
    swRes.headers.get("service-worker-allowed") === "/",
    `got ${swRes.headers.get("service-worker-allowed")}`
  );

  const browser = await chromium.launch({ executablePath: chromiumPath(), headless: true });
  // An explicit context so a second tab can share this one's Service Worker registration and
  // OPFS — a fresh `browser.newPage()` context forbids extra pages, and a separate context
  // would get its own SW, defeating the multi-tab test below.
  const context = await browser.newContext();
  const page = await context.newPage();
  page.on("console", (m) => {
    if (m.type() === "error") console.log("      [console.error]", m.text());
  });
  page.on("pageerror", (e) => console.log("      [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/`);
  await waitReady(page);

  // ---- 1. Mount resolves with the page's origin ---------------------------
  await luaResult(
    page,
    `
    btv.http.mount({
      name = "demo",
      on_request = function(req, respond)
        if req.path == "/" then
          respond({ headers = { ["content-type"] = "text/html" }, body = "<h1>from lua</h1>" })
        elseif req.path == "/echo" then
          respond({
            headers = { ["content-type"] = "application/json" },
            body = btv.json.encode({
              method = req.method, path = req.path, name = req.name,
              q = req.query.v, body = req.body,
            }),
          })
        else
          respond({ status = 404, body = "no such page" })
        end
      end,
    }):next(function(m)
      _G.demo_url = m:url()
      _G.demo = m
    end, function(err) _G.demo_url = "ERROR: " .. tostring(err.message) end)
    `
  );
  // Built here, and asserted to be what the plugin was handed — so a wrong URL fails as
  // itself rather than making every fetch below mysteriously miss.
  const url = `${ORIGIN}/plugin/demo/`;
  const reported = await pollGlobal(page, "demo_url");
  check("btv.http.mount resolves on the browser build", !reported.includes("ERROR"), reported);
  check(
    "mount:url() is the page's own origin + /plugin/<name>/",
    reported.includes(url),
    `expected ${url}, got ${reported}`
  );

  // ---- 2. A real fetch of the mount, through the Service Worker -----------
  // This is the whole point: an ORDINARY browser fetch of an ordinary URL, resolved by Lua.
  const got = await page.evaluate(async (u) => {
    const res = await fetch(u, { cache: "no-store" });
    return { status: res.status, type: res.headers.get("content-type"), body: await res.text() };
  }, url);
  check("a real fetch of the mount reaches Lua", got.status === 200, JSON.stringify(got));
  check("the handler's body arrives verbatim", got.body === "<h1>from lua</h1>", JSON.stringify(got.body));
  check("the handler's content-type arrives", (got.type || "").includes("text/html"), got.type);

  // ---- 3. Request fidelity: method / mount-relative path / query / body ---
  const echo = await page.evaluate(async (u) => {
    const res = await fetch(u + "echo?v=42", {
      method: "POST",
      body: "hello=world",
      cache: "no-store",
    });
    return { status: res.status, json: await res.json() };
  }, url);
  check("POST reaches the handler", echo.status === 200, JSON.stringify(echo));
  check("req.method survives the relay", echo.json && echo.json.method === "POST", JSON.stringify(echo.json));
  check(
    "req.path is MOUNT-RELATIVE (same as native)",
    echo.json && echo.json.path === "/echo",
    JSON.stringify(echo.json)
  );
  check("req.query is decoded", echo.json && echo.json.q === "42", JSON.stringify(echo.json));
  check("req.body survives the relay", echo.json && echo.json.body === "hello=world", JSON.stringify(echo.json));
  check("req.name is the mount", echo.json && echo.json.name === "demo", JSON.stringify(echo.json));

  // ---- 4. The plugin's own 404 -------------------------------------------
  const pluginMiss = await page.evaluate(
    (u) => fetch(u + "nope", { cache: "no-store" }).then((r) => r.status),
    url
  );
  check("the handler's own 404 reaches the browser", pluginMiss === 404, `status ${pluginMiss}`);

  // ---- 5. The EDITOR's 404 for an unmounted name --------------------------
  // Answered by Rust without entering Lua — exactly as the native listener does.
  const editorMiss = await page.evaluate(
    (o) => fetch(o + "/plugin/absent/", { cache: "no-store" }).then((r) => r.status),
    ORIGIN
  );
  check("an unmounted name 404s from the editor", editorMiss === 404, `status ${editorMiss}`);

  // ---- 6. A URL the browser itself loads ---------------------------------
  // The reason this is a Service Worker and not postMessage plumbing: an <iframe> must be
  // able to load a mount as an ordinary document.
  const framed = await page.evaluate(async (u) => {
    const f = document.createElement("iframe");
    f.src = u;
    document.body.appendChild(f);
    await new Promise((res) => {
      f.onload = res;
      setTimeout(res, 5000);
    });
    try {
      return f.contentDocument.body.innerHTML.trim();
    } catch (e) {
      return "ERR: " + e.message;
    }
  }, url);
  check("an <iframe> loads the mount as a real document", framed === "<h1>from lua</h1>", framed);

  // ---- 7. opts.timeout is honored ON THE WEB ------------------------------
  // The point of moving the deadline into Lua: the browser build has no listener to enforce
  // `opts.timeout`, but Lua arms the deadline itself, so the contract is identical here. A
  // 250ms timeout must 504 in well under the Service Worker's 5-minute transport backstop —
  // if the backstop were what answered, this would hang the run instead of passing.
  await luaResult(
    page,
    `
    btv.http.mount({
      name = "silent",
      timeout = 250,
      on_request = function(req, respond) end,   -- never responds
    }):next(function(m) _G.silent_url = m:url() end)
    `
  );
  await pollGlobal(page, "silent_url");
  const started = Date.now();
  const timedOut = await page.evaluate(
    (o) => fetch(o + "/plugin/silent/", { cache: "no-store" }).then((r) => r.status),
    ORIGIN
  );
  const elapsed = Date.now() - started;
  check("a silent handler 504s at opts.timeout (Lua's deadline, not the backstop)", timedOut === 504, `status ${timedOut}`);
  check("…and at the mount's own deadline, not 5 minutes later", elapsed < 15000, `took ${elapsed}ms`);

  // ---- bare mount root redirects to the slash form -----------------------
  // Opening /plugin/demo (no slash) must 308 to /plugin/demo/, or a page served there would
  // resolve its relative URLs (fetch("source")) against /plugin/ and 404. redirect:"manual"
  // so we see the 308 itself rather than the browser silently following it.
  const redir = await page.evaluate(async (o) => {
    const r = await fetch(o + "/plugin/demo", { cache: "no-store", redirect: "manual" });
    return { type: r.type, status: r.status };
  }, ORIGIN);
  // A "manual"-redirect fetch reports an opaqueredirect response (status 0) when a redirect
  // was returned — which, followed, lands on the slash form.
  check(
    "bare mount root redirects (so relative URLs resolve against the mount)",
    redir.type === "opaqueredirect",
    JSON.stringify(redir)
  );
  // And following it (the default) lands on the page.
  const followed = await page.evaluate(
    (o) => fetch(o + "/plugin/demo", { cache: "no-store" }).then((r) => r.text()),
    ORIGIN
  );
  check("…and following it serves the page", followed === "<h1>from lua</h1>", JSON.stringify(followed));

  // ---- 8. a SEPARATE tab loads the mount (the real user path) -------------
  // The whole point of a real URL is opening it in another tab. That tab is a focused,
  // same-origin window client — so a Service Worker that picked the relay target by focus
  // would relay to the requesting tab itself (no editor there) and the load would spin
  // forever. This is the regression guard for exactly that: the request must reach the
  // EDITOR tab, never the one that asked.
  const otherTab = await context.newPage();
  await otherTab.goto(`${ORIGIN}/web/health-probe-not-the-editor`).catch(() => {}); // a same-origin, non-editor page
  await otherTab.bringToFront(); // the editor tab is now backgrounded, this one is focused
  const fromOtherTab = await otherTab.evaluate(async (u) => {
    const r = await Promise.race([
      fetch(u, { cache: "no-store" }).then((res) => res.text().then((b) => ({ status: res.status, body: b }))),
      new Promise((res) => setTimeout(() => res({ status: 0, body: "TIMEOUT" }), 10000)),
    ]);
    return r;
  }, url);
  check(
    "a focused non-editor tab still reaches the editor (no spin)",
    fromOtherTab.status === 200 && fromOtherTab.body === "<h1>from lua</h1>",
    JSON.stringify(fromOtherTab)
  );
  await otherTab.close();

  // ---- 9. mount:close() stops it -----------------------------------------
  await luaResult(page, `_G.demo:close(); return true`);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);
  const closed = await page.evaluate(
    (u) => fetch(u, { cache: "no-store" }).then((r) => r.status),
    url
  );
  check("a closed mount stops answering", closed === 404, `status ${closed}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS — btv.http.mount serves through the Service Worker" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
