// Playwright verifier for the GREP PICKER on the pure web client (serverless, no daemon).
// `nx.picker`'s `live_grep` streams `rg --vimgrep`, falling back to `grep` and then to a
// transport-agnostic `nx.fs.grep` (walk + in-Lua substring match). On the serverless build
// there is no process host, so the rg/grep spawns fail loud (code -1) and the picker must
// land on the nx.fs match over OPFS — the bug was that it showed nothing. This seeds files
// under the cwd, opens live_grep, types a query, and asserts the matching lines appear.
//
// Faithfulness: the files are seeded through the same nx.fs/OPFS seam nx.fs.grep reads, the
// picker runs its real dynamic source through the production tick, and the assertion reads
// the picker's live candidate list (`nx._picker.items`).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-grep-serverless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8147;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

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

// Poll the picker's live candidate list until it contains a NEEDLE match (or timeout).
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
    if (/NEEDLE/.test(s)) return s;
    if (Date.now() - start > ms) return s;
    await sleep(60);
  }
}

const ROOT_REL = `greptest-${Date.now()}`;

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

  await page.goto(`http://localhost:${PORT}/web/`); // serverless — no process host
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless — no process host)", true);

  // Seed files under the cwd: one with the needle on two lines, one without.
  const seeded = await settle(page, "__seed", `nx.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       nx.await(nx.fs.mkdir(base .. "/sub", { recursive = true }))
       nx.await(nx.fs.write(base .. "/a.txt", "alpha NEEDLE one\\nbeta\\n"))
       nx.await(nx.fs.write(base .. "/sub/b.txt", "no match\\nx NEEDLE y\\n"))
       nx.await(nx.fs.write(base .. "/c.txt", "nothing here\\n"))
       _G.__seed = "ok"
     end)()`);
  check("seeded files under cwd via nx.fs (OPFS)", /ok/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  // Open live_grep and type the query — the dynamic source re-runs and, with no rg/grep,
  // falls back to nx.fs.grep, which must surface both NEEDLE lines.
  await luaResult(page, `nx.picker.open('live_grep')`);
  await page.evaluate(() => window.__nxvim.feed("NEEDLE"));
  const items = await pollPickerItems(page);
  const hits = String(items).split("\n").filter((l) => /NEEDLE/.test(l));
  check(
    "live_grep falls back to nx.fs.grep and lists the matching lines (no rg/grep)",
    hits.length >= 2 && hits.some((l) => /a\.txt/.test(l)) && hits.some((l) => /b\.txt/.test(l)),
    `hits=${JSON.stringify(hits)}`,
  );

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
  ? "\nALL PASS — serverless grep picker lists matches via the nx.fs.grep fallback"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
