// Focused verifier for the `'timeout'` / `'timeoutlen'` mapping-timeout on the web
// edit-host: the idle flush that resolves a withheld mapped prefix rides the same
// timer wheel the Worker parks on, so it fires after `timeoutlen` ms — and under
// `:set notimeout` it never fires (a which-key popup stays up). Drives the real
// wasm edit-host in headless Chromium.
//
//   node verify-timeout.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8101;

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

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Map `ggh` so `gg` is a withheld live prefix (not a complete built-in the oracle
  // releases). Track each `ggh` firing in a global so we can prove the prefix was
  // *held* and later completed, not silently dropped. Short timeoutlen for speed.
  await page.evaluate(() => window.__nxvim.execLua(
    "_G.GGH = 0\n" +
    "vim.o.timeoutlen = 150\n" +
    "vim.keymap.set('n', 'ggh', function() _G.GGH = _G.GGH + 1 end)"));
  // Three lines; cursor lands on the last (row 2, 0-based).
  await page.evaluate(() => window.__nxvim.feed("iline1<CR>line2<CR>line3<Esc>"));
  let cur = await page.evaluate(() => window.__nxvim.cursor());
  check("setup: cursor on the last line", cur && cur.row === 2, JSON.stringify(cur));

  // ---- 1. default timeout: the idle flush fires after timeoutlen ----
  await page.evaluate(() => window.__nxvim.feed("gg"));
  cur = await page.evaluate(() => window.__nxvim.cursor());
  check("timeout: gg is withheld (go-to-top hasn't fired yet)", cur && cur.row === 2, JSON.stringify(cur));
  await sleep(500); // > timeoutlen: the Worker wakes and flushes the withheld gg
  cur = await page.evaluate(() => window.__nxvim.cursor());
  check("timeout: the idle flush replayed gg → go-to-top (row 0)", cur && cur.row === 0, JSON.stringify(cur));

  // ---- 2. notimeout: the idle flush never fires; the prefix waits forever ----
  await page.evaluate(() => window.__nxvim.feed("G")); // back to the last line
  cur = await page.evaluate(() => window.__nxvim.cursor());
  check("setup: back on the last line", cur && cur.row === 2, JSON.stringify(cur));
  await page.evaluate(() => window.__nxvim.execLua("vim.o.timeout = false"));

  await page.evaluate(() => window.__nxvim.feed("gg"));
  await sleep(500); // well past timeoutlen
  cur = await page.evaluate(() => window.__nxvim.cursor());
  check("notimeout: gg stays withheld across the wait (no flush, row still 2)", cur && cur.row === 2, JSON.stringify(cur));

  // The next key still disambiguates: `h` completes `ggh` (GGH increments). If the
  // prefix had been dropped, `gg` would have moved to row 0 and `h` alone wouldn't
  // match `ggh` — so GGH==1 proves the prefix was held across the wait.
  await page.evaluate(() => window.__nxvim.feed("h"));
  const ggh = await page.evaluate(() => window.__nxvim.execLua("return _G.GGH"));
  check("notimeout: the next key completed the held ggh map", (ggh?.result || "").includes("1"), JSON.stringify(ggh));

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
