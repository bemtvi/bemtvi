// Focused verifier for the completion popup + fuzzy picker CHROME colors in the web
// client. The native menu widget must resolve its colors from the well-known plugin
// highlight groups (nvim-cmp's `Pmenu`/`PmenuSel`/`CmpItemAbbrMatch` for completion;
// telescope's `Telescope*` for the picker) so a colorscheme themes it automatically,
// rather than the old hardcoded `.pmenu*` CSS. Drives the real wasm edit-host in
// headless Chromium.
//
//   node verify-menu-color.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8097;

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// Test colors, as the rgb() string getComputedStyle returns.
const SEL = "rgb(49, 50, 68)";      // #313244  TelescopeSelection / PmenuSel bg
const MATCH = "rgb(249, 226, 175)"; // #f9e2af  TelescopeMatching / CmpItemAbbrMatch fg
const PBG = "rgb(30, 30, 46)";      // #1e1e2e  Pmenu bg

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

  // ---- Picker (telescope groups) ----
  await page.evaluate(() => window.__nxvim.execLua(
    "vim.api.nvim_set_hl(0, 'TelescopeSelection', { bg = '#313244' })\n" +
    "vim.api.nvim_set_hl(0, 'TelescopeMatching',  { fg = '#f9e2af', bold = true })\n" +
    "nx.picker.source { name = 'fruits', items = function(ctx)\n" +
    "  for _, t in ipairs({'apple','apricot','banana'}) do ctx.push{ text = t } end\n" +
    "end }\n" +
    "nx.picker.open('fruits')"));
  await sleep(200);
  await page.evaluate(() => window.__nxvim.feed("ap"));
  await sleep(200);

  const pick = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")];
    // The selected (first) row carries the selection background.
    const selRow = rows.find((r) => getComputedStyle(r).backgroundColor === "rgb(49, 50, 68)") || rows[0];
    const match = box.querySelector(".row span");
    return {
      selBg: selRow ? getComputedStyle(selRow).backgroundColor : null,
      matchFg: match ? getComputedStyle(match).color : null,
    };
  });
  check("picker: box paints", pick !== null, JSON.stringify(pick));
  if (pick) {
    check("picker: selected row uses TelescopeSelection bg", pick.selBg === SEL, JSON.stringify(pick));
    check("picker: matched chars use TelescopeMatching fg", pick.matchFg === MATCH, JSON.stringify(pick));
  }

  // Close the picker.
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await sleep(150);

  // ---- Completion popup (nvim-cmp groups) ----
  await page.evaluate(() => window.__nxvim.execLua(
    "vim.api.nvim_set_hl(0, 'Pmenu',            { bg = '#1e1e2e' })\n" +
    "vim.api.nvim_set_hl(0, 'PmenuSel',         { bg = '#313244' })\n" +
    "vim.api.nvim_set_hl(0, 'CmpItemAbbrMatch', { fg = '#f9e2af', bold = true })\n" +
    "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } } }"));
  await sleep(100);
  // Seed a word, then start completing its prefix so the popup opens with a match.
  await page.evaluate(() => window.__nxvim.feed("ihello he"));
  await sleep(250);

  const comp = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    const match = box.querySelector(".row span");
    return {
      bg: getComputedStyle(box).backgroundColor,
      matchFg: match ? getComputedStyle(match).color : null,
    };
  });
  check("completion: popup paints", comp !== null, JSON.stringify(comp));
  if (comp) {
    check("completion: popup uses Pmenu bg", comp.bg === PBG, JSON.stringify(comp));
    check("completion: matched chars use CmpItemAbbrMatch fg", comp.matchFg === MATCH, JSON.stringify(comp));
  }

  // Close the completion popup + its insert mode.
  await page.evaluate(() => window.__nxvim.feed("<Esc><Esc>"));
  await sleep(150);

  // ---- Cmdline wildmenu docs float (CmpDocumentation group + wrap) ----
  const DOCBG = "rgb(17, 17, 27)"; // #11111b  CmpDocumentation bg
  const longDesc =
    "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod " +
    "tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam";
  await page.evaluate((desc) => window.__nxvim.execLua(
    "vim.api.nvim_set_hl(0, 'CmpDocumentation', { bg = '#11111b' })\n" +
    "nx.cmdline_complete.setup {}\n" +
    "nx.user_command.create('Wrapcmd', function() end, { desc = [[" + desc + "]] })"), longDesc);
  await sleep(100);
  await page.evaluate(() => window.__nxvim.feed(":Wrapcmd<Tab>"));
  await sleep(200);
  await page.evaluate(() => window.__nxvim.feed("<Tab>")); // select row 0, arming docs
  await sleep(200);

  const docs = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu-doc");
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")];
    // The widest rendered row's text length (every row is padded to the box width).
    const widths = rows.map((r) => r.textContent.length);
    return { bg: getComputedStyle(box).backgroundColor, rowCount: rows.length, maxW: Math.max(...widths, 0) };
  });
  check("docs: float paints", docs !== null, JSON.stringify(docs));
  if (docs) {
    check("docs: uses CmpDocumentation bg", docs.bg === DOCBG, JSON.stringify(docs));
    check("docs: long desc wrapped onto multiple rows", docs.rowCount > 3, JSON.stringify(docs));
    check("docs: every row fits the box width (<=60)", docs.maxW > 0 && docs.maxW <= 60, JSON.stringify(docs));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
