// Playwright verifier for the FILE + GREP pickers on the **python-demo** site (build-demo.sh →
// demo-site/), where a local in-browser process host is installed (build-config `localHost:true`).
//
// The `files` / `live_grep` sources walk a fallback chain — `rg` → `find`/`grep` → a
// transport-agnostic `btv.fs` walk — stepping on only when the previous tool COULD NOT RUN
// (`stream:exit().code == -1`), because zero results is a legitimate answer a re-search must not
// second-guess. The serverless build has no process host at all, so the spawns fail loud with
// `-1` and the walk runs (verify-picker-{files,grep}-serverless.mjs). The demo build DOES have a
// host — one that runs `python` and nothing else — and the bug was that it answered `rg` with a
// shell's command-not-found (127). That reads as "ran, listed nothing", so the chain settled on
// its first step and both pickers came up permanently empty. The host now reports what actually
// happened: a spawn failure (`-1`), the status `btv.run`'s own contract documents.
//
// Faithfulness (not a no-op): nothing is stubbed. The pickers run their real async sources through
// the production tick against the demo's own seeded project in OPFS, and the assertions read the
// picker's live candidate list (`btv._picker.items`) after opening each one through its real
// keymap (`<leader>ff` / `<leader>fg`). The unavailable-binary status is read off a
// real `btv.run_stream` of `rg`, and the "no CPython" check watches the page's actual network
// requests. Mutation-tested: restoring the 127 status fails checks 2/4/5.
//
// Prereqs: ./build-demo.sh (demo-site/ assembled) and a Chromium for Playwright
// (PW_CHROMIUM=/path/to/chrome on macOS).
// Run:  node verify-picker-demo.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8171;

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

// Run a chain that stashes its outcome in `_G.<g>`, then poll until that global settles.
// execLua renders the result through rmpv's Debug, so an unset global comes back as
// `…Ok("nil")…`; poll until that's gone (the chain resolved or rejected).
async function settle(page, g, code, ms = 15000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await luaResult(page, `return tostring(_G.${g})`);
    if (!/Ok\("nil"\)/.test(String(v))) return String(v);
    if (Date.now() - start > ms) return String(v);
    await sleep(60);
  }
}

// Poll the picker's live candidate list until it matches `want` (or times out). A `live_grep` row
// is two-column — the `path:line:col: ` head plus the matched line — so the rendered label is
// their concatenation.
async function pollPickerItems(page, want, ms = 15000) {
  const start = Date.now();
  for (;;) {
    const v = await luaResult(
      page,
      `local p = btv._picker\n` +
        `if not p then return "NOPICKER" end\n` +
        `local t = {}\n` +
        `for i = 1, (p.nitems or 0) do\n` +
        `  local it = p.items[i]\n` +
        `  t[#t + 1] = (it.head or "") .. it.text\n` +
        `end\n` +
        `return table.concat(t, "\\n")`,
    );
    const s = String(v);
    if (want.test(s)) return s;
    if (Date.now() - start > ms) return s;
    await sleep(100);
  }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, BEMTVI_SERVE_ROOT: `${here}../demo-site` },
});
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
  // Watch for the CPython download: an unavailable binary must be answered by the host itself,
  // never by waking the interpreter (a picker keypress would otherwise pull ~10MB of wasm).
  let pyodideFetched = false;
  page.on("request", (r) => { if (/\/vendor\/pyodide\//.test(r.url())) pyodideFetched = true; });

  // No `?daemon=` — the demo's own serverless boot, with the local Pyodide process host.
  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__bemtvi.ready);
  // The demo seeds its project into OPFS and sources init.lua on first boot; give that its tick.
  const seeded = await settle(page, "__files", `btv.fs.walk("/", { hidden = true })
       :next(function(f) _G.__files = #f end, function(e) _G.__files = "err:" .. tostring(e.code) end)`);
  check("demo booted with its seeded project in OPFS", /Ok\("[1-9]/.test(seeded), `walk=${seeded}`);

  // 1) A process host IS installed here (this is what makes the demo different from serverless).
  const host = await luaResult(page, `return tostring(btv.run ~= nil)`);
  check("the local process host is installed (btv.run available)", /true/.test(String(host)), `host=${host}`);

  // 2) …and a binary it cannot run reports a SPAWN FAILURE (`-1`), not a fabricated "ran" status.
  const rg = await settle(page, "__rg", `btv.async(function()
       local s = btv.run_stream({ cmd = "rg", args = { "--files" }, cwd = "/" })
       for _ in btv.await_each(s) do end
       local e = s:exit()
       _G.__rg = tostring(e and e.code)
     end)()`);
  check("an unavailable binary (`rg`) reports exit -1 — the tool never RAN", /Ok\("-1"\)/.test(rg), `rg exit=${rg}`);

  // 3) …answered by the host itself, without booting CPython to say "no rg".
  check("answering it did not download/boot Pyodide", pyodideFetched === false);

  // 4) The `files` picker therefore reaches the btv.fs walk and lists the demo's project files.
  // Opened the way a visitor opens it — the prelude's default `<leader>ff`, with the demo's
  // `mapleader = " "` — so the check covers the keymap path, not just the API.
  await page.evaluate(() => window.__bemtvi.feed("<Space>ff"));
  const items = await pollPickerItems(page, /main\.py/);
  check(
    "files picker falls through rg/find to the btv.fs walk and lists the project",
    /main\.py/.test(items) && /TOUR\.md/.test(items),
    `items=${JSON.stringify(items).slice(0, 400)}`,
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(200);

  // 5) …and live_grep reaches btv.fs.grep, whose hits carry the `path:line:col: ` head. `import`
  // appears in the seeded python sources and nowhere in this query's own machinery.
  await page.evaluate(() => window.__bemtvi.feed("<Space>fg"));
  await sleep(200);
  await page.evaluate(() => window.__bemtvi.feed("import"));
  const hits = await pollPickerItems(page, /\.py:\d+:\d+: /);
  check(
    "live_grep falls through rg/grep to btv.fs.grep and lists located hits",
    /\.py:\d+:\d+: /.test(hits) && /import/.test(hits),
    `hits=${JSON.stringify(hits).slice(0, 400)}`,
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
} catch (e) {
  check("harness ran without throwing", false, String((e && e.stack) || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — the demo's pickers fall back past its python-only process host"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
