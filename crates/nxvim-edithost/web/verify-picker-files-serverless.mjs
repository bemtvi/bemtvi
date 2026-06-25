// Playwright verifier for the FILE PICKER on the pure web client (serverless, no daemon).
// The `nx.picker` `files` source streams `rg --files`, falling back to `find` and then to a
// transport-agnostic `nx.fs` walk. On the serverless build there is no process host, so the
// rg/find spawns fail loud (code -1) and the picker must land on the nx.fs walk over OPFS —
// the bug was that it showed nothing. This seeds a couple of files under the cwd via nx.fs,
// opens the `files` picker, and asserts they appear in the picker's candidate list.
//
// Faithfulness (not a no-op): the files are seeded through the SAME nx.fs/OPFS seam the walk
// reads, the picker runs its real async source through the production tick, and the assertion
// reads the picker's live candidate list (`nx._picker.items`).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-files-serverless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8146;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
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
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// Run a chain that stashes its outcome in `_G.<g>`, then poll until that global settles.
// execLua renders the result through rmpv's Debug, so an unset global comes back as
// `…Ok("nil")…`; poll until that's gone (the chain resolved or rejected).
async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__nxvim.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Poll the picker's live candidate list until the seeded files show up (or timeout).
async function pollPickerItems(page, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await luaResult(
      page,
      `local p = nx._picker\n` +
        `if not p then return "NOPICKER" end\n` +
        `local t = {}\n` +
        `for i = 1, (p.nitems or 0) do t[#t + 1] = p.items[i].text end\n` +
        `return table.concat(t, "\\n")`,
    );
    const s = String(v);
    if (/alpha\.txt/.test(s) && /beta\.txt/.test(s)) return s;
    if (Date.now() - start > ms) return s;
    await sleep(60);
  }
}

const ROOT_REL = `picktest-${Date.now()}`;

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

  // No `?daemon=` — serverless. There is no process host, so rg/find can't run.
  await page.goto(`http://localhost:${PORT}/web/`);

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless — no daemon, no process host)", true);

  // Seed two files under the cwd through the nx.fs/OPFS seam the walk will read back.
  const seeded = await settle(page, "__seed", `nx.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       nx.await(nx.fs.mkdir(base .. "/sub", { recursive = true }))
       nx.await(nx.fs.write(base .. "/alpha.txt", "a"))
       nx.await(nx.fs.write(base .. "/sub/beta.txt", "b"))
       _G.__seed = "ok"
     end)()`);
  check("seeded alpha.txt + sub/beta.txt under cwd via nx.fs (OPFS)", /ok/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  // Open the real `files` picker — rg/find spawns fail loud (no host), so it must fall back
  // to the nx.fs walk and surface the seeded files.
  await luaResult(page, `nx.picker.open('files')`);
  const items = await pollPickerItems(page);
  check(
    "file picker falls back to the nx.fs walk and lists the seeded files (no rg/find)",
    /alpha\.txt/.test(String(items)) && /beta\.txt/.test(String(items)),
    `items=${JSON.stringify(items)}`,
  );

  // Close the picker and clean up the seeded subtree.
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await settle(page, "__rm", `nx.fs.remove(vim.fn.getcwd() .. "/${ROOT_REL}", { recursive = true })
       :next(function() _G.__rm = "gone" end, function(e) _G.__rm = "err:" .. e.code end)`);
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless file picker lists files via the nx.fs walk fallback"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
