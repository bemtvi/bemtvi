// Focused verifier for float CHROME colors (FloatBorder / NormalFloat / FloatTitle)
// in the web client: a colorscheme that defines those groups must color the float
// border glyphs, title, and background — not just draw them in the hardcoded CSS
// default. Drives the real wasm edit-host in headless Chromium.
//
//   node verify-float-color.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8099;

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// The three test colors, as the rgb() string getComputedStyle returns.
const BORDER = "rgb(255, 136, 0)";   // #ff8800
const TITLE = "rgb(136, 255, 0)";    // #88ff00
const BG = "rgb(34, 34, 51)";        // #222233

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // Define the float chrome groups, then open a rounded titled content float.
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, 'FloatBorder', { fg = '#ff8800', bg = '#222233' })\n" +
    "vim.api.nvim_set_hl(0, 'NormalFloat', { fg = '#cdd6f4', bg = '#222233' })\n" +
    "vim.api.nvim_set_hl(0, 'FloatTitle',  { fg = '#88ff00', bg = '#222233' })\n" +
    "btv.ui.float('hello float\\nsecond line', { title = 'info', border = 'rounded' })"));
  await sleep(200);

  const cf = await page.evaluate(() => {
    const box = document.querySelector("#grid .popup-chrome");
    if (!box) return null;
    const border = box.querySelector(".float-border");
    const title = box.querySelector(".float-title");
    return {
      borderColor: border ? getComputedStyle(border).color : null,
      titleColor: title ? getComputedStyle(title).color : null,
      bg: getComputedStyle(box).backgroundColor,
    };
  });
  check("content float: box paints", cf !== null, JSON.stringify(cf));
  if (cf) {
    check("content float: border glyphs use FloatBorder fg", cf.borderColor === BORDER, JSON.stringify(cf));
    check("content float: title uses FloatTitle fg", cf.titleColor === TITLE, JSON.stringify(cf));
    check("content float: box uses NormalFloat bg", cf.bg === BG, JSON.stringify(cf));
  }

  // A window float (real buffer in a float) goes through renderFloat — same chrome.
  await page.evaluate(() => window.__bemtvi.execLua(
    "btv.view.component({\n" +
    "  setup = function() return {} end,\n" +
    "  render = function() return { lines = { 'alpha', 'beta', 'gamma' } } end,\n" +
    "}).mount({ name = 'vfc', filetype = 'vfc',\n" +
    "  float = { width = 24, height = 3, border = 'rounded', title = 'win float', grab = true } })"));
  await sleep(250);

  const wf = await page.evaluate(() => {
    const chrome = document.querySelector("#grid .float-win");
    if (!chrome) return null;
    const border = chrome.querySelector(".float-border");
    const title = chrome.querySelector(".float-title");
    return {
      borderColor: border ? getComputedStyle(border).color : null,
      titleColor: title ? getComputedStyle(title).color : null,
      bg: getComputedStyle(chrome).backgroundColor,
    };
  });
  check("window float: box paints", wf !== null, JSON.stringify(wf));
  if (wf) {
    check("window float: border glyphs use FloatBorder fg", wf.borderColor === BORDER, JSON.stringify(wf));
    check("window float: title uses FloatTitle fg", wf.titleColor === TITLE, JSON.stringify(wf));
    check("window float: box uses NormalFloat bg", wf.bg === BG, JSON.stringify(wf));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
