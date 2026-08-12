// Playwright verifier for SERVERLESS `btv.fs` over OPFS (Phase 3 of the off-tick plan). With NO
// daemon connected, a browser `btv.fs.*` op runs against the Origin Private File System in the
// Worker — the JS twin of the daemon's `run_fs_job`. This proves the full op set round-trips
// against OPFS (the same sandbox `:e`/`:w` persist to), so a plugin's `btv.fs` works in the common
// no-daemon browser mode, not just with a daemon (verify-fs-op.mjs covers the daemon path).
//
// Faithfulness (not a no-op): every op is exercised end-to-end through the real `btv.fs` promise
// surface and asserted against what a *sibling* op observes (write→read, mkdir→readdir→stat,
// copy→read, rename→exists, remove→exists), plus the error envelope (ENOENT) round-trips. All
// state lives in OPFS — there is no daemon and no daemon disk.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-fs-op-serverless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8143;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);
// Run a `btv.fs` chain that stashes its outcome in `_G.<g>`, then poll until that global settles.
// execLua renders its return through rmpv's Debug, so `tostring(_G.<g>)` of an unset global comes
// back as `…Ok("nil")…`; poll until that's gone (the chain resolved or rejected — either sets the
// global), then the caller asserts the rendered content.
async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  // `g` must cross into the browser as a page.evaluate ARGUMENT — a browser closure can't
  // capture this Node-side variable (that's what tripped the first cut: `_G.undefined`).
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__bemtvi.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// A fresh OPFS subtree per run (OPFS persists per-origin across runs) so the assertions are
// deterministic — no `Math.random` in the worker, just the test harness's clock.
const ROOT = `/fsop-${Date.now()}`;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // No `?daemon=` — serverless. `btv.fs` must route to OPFS, not fail loud.
  await page.goto(`http://localhost:${PORT}/web/`);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted (serverless — no daemon)", true);

  const file = `${ROOT}/hello.txt`;

  // ── 1. write then read_text round-trips through OPFS ──────────────────────────────────────
  const wrote = await settle(page, "__w", `btv.fs.mkdir("${ROOT}", { recursive = true }):next(function()
       btv.fs.write("${file}", "HELLO-FROM-OPFS\\nsecond"):next(function() _G.__w = "wrote" end,
         function(e) _G.__w = "werr:" .. e.code end)
     end, function(e) _G.__w = "mkerr:" .. e.code end)`);
  check("serverless btv.fs.mkdir + write resolve against OPFS", /wrote/.test(String(wrote)), `w=${JSON.stringify(wrote)}`);
  const text = await settle(page, "__text",
    `btv.fs.read_text("${file}"):next(function(t) _G.__text = t end, function(e) _G.__text = "ERR:"..e.code end)`);
  check("serverless btv.fs.write + read_text round-trip through OPFS",
    /HELLO-FROM-OPFS/.test(String(text)) && /second/.test(String(text)), `text=${JSON.stringify(text)}`);

  // ── 2. mkdir + readdir lists entries with kinds (file vs directory) ───────────────────────
  // (execLua renders the result via rmpv Debug, so assertions match the content WITHIN the
  // rendered `Ok("…")` wrapper — substring matches, not `^…$` anchors.)
  const mk = await settle(page, "__mk", `btv.fs.mkdir("${ROOT}/sub", { recursive = true }):next(
       function() _G.__mk = "made" end, function(e) _G.__mk = "err:"..e.code end)`);
  check("serverless btv.fs.mkdir creates a subdirectory", /made/.test(String(mk)), `mk=${JSON.stringify(mk)}`);
  const names = await settle(page, "__names", `btv.fs.readdir("${ROOT}"):next(function(entries)
       local out = {}
       for _, e in ipairs(entries) do out[#out+1] = e.name..":"..e.type end
       table.sort(out)
       _G.__names = "[" .. table.concat(out, ",") .. "]"
     end, function(e) _G.__names = "ERR:"..e.code end)`);
  check("serverless btv.fs.readdir lists OPFS entries with kinds",
    /hello\.txt:file/.test(String(names)) && /sub:directory/.test(String(names)), `names=${JSON.stringify(names)}`);

  // ── 3. stat reports type + size of the written file ───────────────────────────────────────
  const stat = await settle(page, "__stat", `btv.fs.stat("${file}"):next(function(s)
       _G.__stat = "S[" .. s.type .. ":" .. tostring(s.size) .. "]"
     end, function(e) _G.__stat = "ERR:"..e.code end)`);
  // "HELLO-FROM-OPFS\nsecond" = 22 bytes.
  check("serverless btv.fs.stat reports type=file and the byte size",
    /S\[file:22\]/.test(String(stat)), `stat=${JSON.stringify(stat)}`);

  // ── 4. copy duplicates a file; the copy reads back identical ──────────────────────────────
  const copied = await settle(page, "__copied", `btv.fs.copy("${file}", "${ROOT}/copy.txt"):next(function()
       btv.fs.read_text("${ROOT}/copy.txt"):next(function(t) _G.__copied = t end, function(e) _G.__copied="rerr:"..e.code end)
     end, function(e) _G.__copied = "cerr:"..e.code end)`);
  check("serverless btv.fs.copy duplicates a file (copy reads back identical)",
    /HELLO-FROM-OPFS/.test(String(copied)), `copied=${JSON.stringify(copied)}`);

  // ── 5. rename moves a file: the old name is gone, the new one reads back ───────────────────
  const renamed = await settle(page, "__ren", `btv.fs.rename("${ROOT}/copy.txt", "${ROOT}/renamed.txt"):next(function()
       btv.fs.exists("${ROOT}/copy.txt"):next(function(oldExists)
         btv.fs.read_text("${ROOT}/renamed.txt"):next(function(t)
           _G.__ren = (oldExists and "OLD-STILL-THERE" or "MOVED") .. ":" .. t
         end, function(e) _G.__ren = "rerr:"..e.code end)
       end)
     end, function(e) _G.__ren = "renerr:"..e.code end)`);
  check("serverless btv.fs.rename moves the file (old gone, new content intact)",
    /MOVED:HELLO-FROM-OPFS/.test(String(renamed)) && !/OLD-STILL-THERE/.test(String(renamed)),
    `renamed=${JSON.stringify(renamed)}`);

  // ── 6. remove deletes a file; exists() then resolves false ────────────────────────────────
  const removed = await settle(page, "__rm", `btv.fs.remove("${ROOT}/renamed.txt"):next(function()
       btv.fs.exists("${ROOT}/renamed.txt"):next(function(ex) _G.__rm = ex and "STILL-EXISTS" or "GONE-OK" end)
     end, function(e) _G.__rm = "rmerr:"..e.code end)`);
  check("serverless btv.fs.remove unlinks the file (exists() then false)",
    /GONE-OK/.test(String(removed)), `removed=${JSON.stringify(removed)}`);

  // ── 7. a missing path REJECTS with err.code == ENOENT (the error envelope round-trips) ─────
  const code = await settle(page, "__code", `btv.fs.read_text("${ROOT}/nope.txt"):next(
       function(_) _G.__code = "RESOLVED?!" end, function(e) _G.__code = e.code end)`);
  check("serverless btv.fs read of a missing path rejects with err.code == ENOENT",
    /ENOENT/.test(String(code)), `code=${JSON.stringify(code)}`);

  await browser.close();
} catch (e) {
  console.error("verify-fs-op-serverless error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless browser btv.fs runs against OPFS (write/read_text/readdir/stat/copy/rename/remove/ENOENT)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
