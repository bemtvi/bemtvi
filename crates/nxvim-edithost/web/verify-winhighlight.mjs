// Focused verifier for per-window `'winhighlight'` chrome in the web client: a dock
// with `winhighlight = 'Normal:NormalSB'` must repaint ITS OWN background with
// NormalSB's color, leaving the main window on the global background. Drives the
// real wasm edit-host in headless Chromium.
//
//   node verify-winhighlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8101;

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// The sidebar background, as the rgb() string getComputedStyle returns.
const SB = "rgb(32, 32, 48)"; // #202030

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Collect every `.win` box background before any winhighlight — none should carry
  // the sidebar color (they inherit the global #grid background, i.e. transparent).
  const before = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win")].map((b) => getComputedStyle(b).backgroundColor));
  check("baseline: no window paints the sidebar bg", !before.includes(SB), JSON.stringify(before));

  // Open a left dock, define NormalSB, and give the dock `winhighlight = Normal:NormalSB`.
  await page.evaluate(() => window.__nxvim.execLua(
    "nx.dock.open{ side = 'left', size = 24 }\n" +
    "nx.hl.define(0, 'NormalSB', { bg = '#202030' })\n" +
    "nx.dock.opt('left').winhighlight = 'Normal:NormalSB'"));
  await sleep(300);

  const after = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win")].map((b) => getComputedStyle(b).backgroundColor));
  const sidebars = after.filter((c) => c === SB).length;
  const others = after.filter((c) => c !== SB).length;
  check("exactly one window (the dock) paints the NormalSB background", sidebars === 1, JSON.stringify(after));
  check("at least one window (the main area) keeps the global background", others >= 1, JSON.stringify(after));

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
