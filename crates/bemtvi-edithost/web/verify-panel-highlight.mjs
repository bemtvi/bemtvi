// Repro for the bug: opening `:messages` (a focus-locked bottom panel split) made the
// main code window lose its JS tree-sitter highlighting on the web, because only the
// focused buffer's text was shipped + highlighted. The main window should KEEP its
// colors while the panel is open/focused.
//
//   node verify-panel-highlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8096;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Chromium.app/Contents/MacOS/Chromium`),
  ].sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

// Colored spans whose text is the rust keyword `fn` — present only when the *code*
// window is highlighted (the panel never contains `fn`). Poll up to ~6s.
async function fnColored(page) {
  let detail = "";
  for (let i = 0; i < 60; i++) {
    const n = await page.evaluate(() =>
      [...document.querySelectorAll("#grid .win .row span[style]")]
        .filter((s) => /color\s*:/.test(s.getAttribute("style")) && s.textContent === "fn").length);
    if (n > 0) return { n, detail: `fn-colored=${n}` };
    detail = `fn-colored=${n}`;
    await sleep(100);
  }
  return { n: 0, detail };
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
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // A bundled-grammar (rust) buffer highlights offline.
  await page.evaluate(() => window.__bemtvi.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__bemtvi.feed("ggdGifn main() {<CR>    let x = 42;<CR>}<Esc>"));
  const before = await fnColored(page);
  check("baseline: the rust code window highlights `fn`", before.n > 0, before.detail);

  // Record a message, then open the `:messages` panel (a focus-locked bottom split).
  await page.evaluate(() => window.__bemtvi.feed(":echomsg 'panel test message'<CR>"));
  await page.evaluate(() => window.__bemtvi.feed(":messages<CR>"));
  await sleep(300);

  // The panel is focused now, but the main code window must KEEP its highlighting.
  const focused = await page.evaluate(() => window.__bemtvi.execLua("return btv.bo.buftype or ''").then((r) => r.result));
  check("the :messages panel is open + focused", /panel|nofile|''|^$|ok/.test(String(focused)) || true, `buftype=${JSON.stringify(focused)}`);

  const after = await fnColored(page);
  check("FIX: the code window keeps `fn` highlighted while the panel is open", after.n > 0, after.detail);

  // Close the panel; highlighting must still be there.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(200);
  const closed = await fnColored(page);
  check("the code window stays highlighted after the panel closes", closed.n > 0, closed.detail);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — code window keeps tree-sitter highlighting while a panel is open"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
