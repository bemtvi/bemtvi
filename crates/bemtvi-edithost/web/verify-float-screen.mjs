// Focused verifier for the `screen` window region in the web client: an
// `editor`-relative float positions against the WHOLE windows area, not the region
// that happens to be focused. With a left dock open, a centered float must still be
// centered on the full grid — the client maps region `"screen"` to the windows-area
// origin instead of the dock-shrunk main region's. Drives the real wasm edit-host in
// headless Chromium.
//
//   node verify-float-screen.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8106;

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
  // `CHROME_PATH` runs against an already-installed Chromium (the Playwright browser
  // download is optional in this repo's setup); unset ⇒ Playwright's own.
  const browser = await chromium.launch(
    process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {});
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // A left dock, then a centered float mounted while that dock holds focus.
  await page.evaluate(() => window.__bemtvi.execLua("btv.dock.open{ side = 'left', size = 20 }"));
  await sleep(150);
  await page.evaluate(() => window.__bemtvi.execLua(
    "btv.view.component({\n" +
    "  setup = function() return {} end,\n" +
    "  render = function() return { lines = { 'CENTERED' } } end,\n" +
    "}).mount({ name = 'scr', filetype = 'scr',\n" +
    "  float = { width = '50%', height = 6, align = 'center', border = 'rounded',\n" +
    "            title = 'screen float', grab = true } })"));
  await sleep(300);

  // Measure in pixels: the float's chrome box against the grid it floats over. Its
  // horizontal center must match the GRID's center (a region-centered float would sit
  // to the right of it, by half the dock band).
  const geom = await page.evaluate(() => {
    const grid = document.querySelector("#grid");
    const chrome = document.querySelector("#grid .float-win");
    if (!grid || !chrome) return null;
    const g = grid.getBoundingClientRect(), c = chrome.getBoundingClientRect();
    // The left-most window box is the dock's, 20 cells wide — the cell width.
    const dock = [...document.querySelectorAll("#grid .win")]
      .map((w) => w.getBoundingClientRect())
      .sort((a, b) => a.left - b.left)[0];
    return {
      gridCenter: g.left + g.width / 2,
      floatCenter: c.left + c.width / 2,
      cellW: dock ? dock.width / 20 : null,
    };
  });
  check("a centered editor float is rendered at all",
    geom !== null && geom.cellW, JSON.stringify(geom));

  if (geom && geom.cellW) {
    // Centered on the whole grid, within a cell of rounding.
    check("centered on the whole screen",
      Math.abs(geom.floatCenter - geom.gridCenter) <= geom.cellW,
      JSON.stringify(geom));
    // Distinctly NOT centered on the main region: that region starts past the dock's
    // 21-cell band (20 content + separator), so its center sits ~10 cells right.
    const regionCenter = geom.gridCenter + (21 * geom.cellW) / 2;
    check("not centered on the dock-shrunk main region",
      geom.floatCenter < regionCenter - 2 * geom.cellW,
      JSON.stringify({ ...geom, regionCenter }));
  }

  await browser.close();
} finally {
  cleanup();
}

process.exit(failures ? 1 : 0);
