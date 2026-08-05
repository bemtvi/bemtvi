// Focused verifier for `'colorcolumn'` in the web client: setting the option must
// paint one thin vertical `.colorcolumn` ruler per configured column down the text
// body (tracking the text under horizontal scroll), and clearing it removes them.
// Drives the real wasm edit-host in headless Chromium.
//
//   node verify-colorcolumn.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8103;

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

// Read the focused window's `.colorcolumn` ruler divs as {left, bg} pairs.
const rulers = () =>
  [...document.querySelectorAll("#grid .win .colorcolumn")].map((d) => ({
    left: parseFloat(d.style.left),
    bg: getComputedStyle(d).backgroundColor,
  }));

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

  // Some content so the rulers span several rows and the columns are meaningful.
  await page.evaluate(() => window.__nxvim.feed("ione two three four five six<CR>seven eight nine ten<Esc>"));
  await sleep(200);

  // Baseline: no rulers before the option is set.
  const before = await page.evaluate(rulers);
  check("baseline: no colorcolumn rulers", before.length === 0, JSON.stringify(before));

  // Set two rulers.
  await page.evaluate(() => window.__nxvim.execLua("vim.opt.colorcolumn = '3,6'"));
  await sleep(200);
  const two = await page.evaluate(rulers);
  check("two rulers appear for colorcolumn=3,6", two.length === 2, JSON.stringify(two));
  check(
    "each ruler carries an opaque tint (not transparent)",
    two.length === 2 && two.every((r) => r.bg && r.bg !== "rgba(0, 0, 0, 0)" && r.bg !== "transparent"),
    JSON.stringify(two),
  );
  check(
    "the column-6 ruler sits to the right of the column-3 ruler",
    two.length === 2 && two[1].left > two[0].left && two[0].left >= 0,
    JSON.stringify(two.map((r) => r.left)),
  );

  // (The ruler colour comes from the `--colorcolumn` CSS var, which `applyChrome`
  // drives from the `ColorColumn` group on a colorscheme change — the same shared
  // path as `--cursorline`, covered by verify-catppuccin. Unthemed, the ruler shows
  // the built-in One Dark fallback tint asserted above.)

  // Clearing the option removes the rulers.
  await page.evaluate(() => window.__nxvim.execLua("vim.opt.colorcolumn = ''"));
  await sleep(200);
  const cleared = await page.evaluate(rulers);
  check("clearing colorcolumn removes the rulers", cleared.length === 0, JSON.stringify(cleared));

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
