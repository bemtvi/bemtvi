// Playwright verifier for the colorscheme → web bridge (docs/plans/2026-06-24-web-
// colorscheme-bridge.md). On the wasm build the editor highlights code JS-side and
// synthesizes its chrome, so a loaded colorscheme (catppuccin, …) used to be ignored —
// the editor stayed One Dark. The bridge ships the resolved colorscheme to the client:
// chrome groups via `view.chrome` (applied to the `:root` CSS vars) and the tree-sitter
// capture groups via `view.theme` (fed to the JS highlighter). This drives the real
// wasm edit-host in headless Chromium and asserts both halves recolor when a scheme is
// loaded — using `nvim_set_hl` directly, so it tests the colorscheme-agnostic mechanism
// (the same path `require("catppuccin").load()` drives).
//
//   node verify-colorscheme.mjs
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

// The color a token `text` renders in, in the focused window's first row (the inline
// `color:` the highlighter set), polled until present. null if never colored.
async function tokenColor(page, text) {
  for (let i = 0; i < 60; i++) {
    const c = await page.evaluate((t) => {
      const row = document.querySelector("#grid .win .row");
      if (!row) return null;
      const run = [...row.querySelectorAll("span")].find((s) => s.textContent === t);
      return run ? (run.getAttribute("style") || "").match(/color\s*:\s*([^;]+)/)?.[1]?.trim() ?? null : null;
    }, text);
    if (c) return c;
    await sleep(100);
  }
  return null;
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

  // A rust buffer (rust is a bundled grammar → highlights offline). `fn` is a keyword,
  // `main` a function name — distinct capture groups to retheme.
  await page.evaluate(() => window.__nxvim.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__nxvim.feed("ggdGifn main() {}<Esc>"));

  // ---- 1. Before any colorscheme: the built-in One Dark family ----
  const kwBefore = await tokenColor(page, "fn");
  const fnBefore = await tokenColor(page, "main");
  check("default: `fn` is One Dark keyword (#c678dd)", kwBefore === "#c678dd", kwBefore);
  check("default: `main` is One Dark function (#61afef)", fnBefore === "#61afef", fnBefore);
  const bgBefore = await page.evaluate(() => getComputedStyle(document.getElementById("grid")).backgroundColor);
  check("default: editor bg is the One Dark default (#1e2127)", bgBefore === "rgb(30, 33, 39)", bgBefore);

  // ---- 2. Load a colorscheme (the catppuccin-mocha-ish palette, set directly) ----
  await page.evaluate(() => window.__nxvim.execLua(
    "vim.api.nvim_set_hl(0, 'Normal',    { fg = '#cdd6f4', bg = '#1e1e2e' })\n" +
    "vim.api.nvim_set_hl(0, 'Visual',    { bg = '#414155' })\n" +
    "vim.api.nvim_set_hl(0, '@keyword',  { fg = '#cba6f7' })\n" +   // mauve
    "vim.api.nvim_set_hl(0, '@function', { fg = '#89b4fa' })\n"));   // blue
  // Nudge a redraw so the new theme/chrome frame ships.
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // ---- 3. Chrome: the editor background follows Normal bg ----
  let bgAfter = "";
  for (let i = 0; i < 60; i++) {
    bgAfter = await page.evaluate(() => getComputedStyle(document.getElementById("grid")).backgroundColor);
    if (bgAfter === "rgb(30, 30, 46)") break;
    await sleep(100);
  }
  check("chrome: editor bg follows Normal bg (#1e1e2e)", bgAfter === "rgb(30, 30, 46)", bgAfter);

  // ---- 4. Syntax: code tokens follow the colorscheme's capture groups ----
  const kwAfter = await tokenColor(page, "fn");
  const fnAfter = await tokenColor(page, "main");
  check("syntax: `fn` recolors to @keyword (#cba6f7)", kwAfter === "#cba6f7", kwAfter);
  check("syntax: `main` recolors to @function (#89b4fa)", fnAfter === "#89b4fa", fnAfter);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — colorscheme bridges to web chrome + syntax"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
