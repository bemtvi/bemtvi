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
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8097;

// The browser Playwright downloaded, wherever this machine keeps it (`PW_CHROMIUM`
// overrides) — the same resolution the other verifiers use, so a cache holding a
// different build number than the pinned package still runs.
function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const pats = [
    `${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`,
    `${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/*.app/Contents/MacOS/*`,
  ];
  for (const p of pats) {
    const found = globSync(p).sort();
    if (found.length) return found[found.length - 1];
  }
  return undefined;
}

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

// Resolve the installed Chromium the way every other verifier here does: this checkout's
// Playwright is in the npx cache, so its default headless-shell path does not exist and a
// bare `chromium.launch()` throws "Executable doesn't exist".
function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

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

  // ---- Picker (telescope groups) ----
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, 'TelescopeSelection', { bg = '#313244' })\n" +
    "vim.api.nvim_set_hl(0, 'TelescopeMatching',  { fg = '#f9e2af', bold = true })\n" +
    "btv.picker.source { name = 'fruits', items = function(ctx)\n" +
    "  for _, t in ipairs({'apple','apricot','banana'}) do ctx.push{ text = t } end\n" +
    "end }\n" +
    "btv.picker.open('fruits')"));
  await sleep(200);
  await page.evaluate(() => window.__bemtvi.feed("ap"));
  await sleep(200);

  const pick = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")];
    // The selected (first) row carries the selection background.
    const selRow = rows.find((r) => getComputedStyle(r).backgroundColor === "rgb(49, 50, 68)") || rows[0];
    // The MATCH span specifically. Not `.row span`: a picker's first row is the prompt,
    // which is a `.row` too, so that finds the prompt's caret span first and reads the
    // cursor's color instead. `pmenu-match` is on every match span unconditionally (see
    // index.html) precisely so it can be identified whether or not a theme styled it.
    const match = box.querySelector(".pmenu-match");
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
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);

  // ---- Painted rows (the diagnostics picker's severity color) ----
  // A source may paint each row with a highlight group (`ctx.push { hl = … }`); the
  // server resolves it per frame and the client colors the row's HEAD column with it,
  // leaving the body in the list's own foreground.
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, 'DiagnosticError', { fg = '#ff0000' })\n" +
    "btv.diagnostic.set(1, 0, { { lnum = 0, col = 0, message = 'boom',\n" +
    "  severity = btv.diagnostic.severity.ERROR } })\n" +
    "btv.picker.open('diagnostics')"));
  await sleep(250);

  const painted = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    const row = [...box.querySelectorAll(".row")].find((r) => r.textContent.includes("boom"));
    if (!row) return null;
    // Every colored piece of the row, in order, with its text.
    const spans = [...row.querySelectorAll("span")].map((sp) => ({
      text: sp.textContent,
      fg: getComputedStyle(sp).color,
    }));
    return { text: row.textContent, spans };
  });
  check("painted: the diagnostics row paints", painted !== null, JSON.stringify(painted));
  if (painted) {
    const red = painted.spans.filter((sp) => sp.fg === "rgb(255, 0, 0)");
    check("painted: the severity head is DiagnosticError red", red.length > 0, JSON.stringify(painted));
    // The head is the classification + location; the message body stays uncolored.
    const redText = red.map((sp) => sp.text).join("");
    check(
      "painted: the head, not the message, carries the color",
      red.length > 0 && !redText.includes("boom"),
      JSON.stringify(painted),
    );
    check("painted: the row leads with the severity tag", /^E /.test(painted.text), JSON.stringify(painted));
  }

  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);

  // ---- Completion popup (nvim-cmp groups) ----
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, 'Pmenu',            { bg = '#1e1e2e' })\n" +
    "vim.api.nvim_set_hl(0, 'PmenuSel',         { bg = '#313244' })\n" +
    "vim.api.nvim_set_hl(0, 'CmpItemAbbrMatch', { fg = '#f9e2af', bold = true })\n" +
    "btv.complete.setup { sources = { { 'buffer', min_chars = 2 } } }"));
  await sleep(100);
  // Seed a word, then start completing its prefix so the popup opens with a match.
  await page.evaluate(() => window.__bemtvi.feed("ihello he"));
  await sleep(250);

  const comp = await page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    // Same explicit match-span selector as the picker above. A completion popup has no
    // prompt row, so `.row span` happened to land on the match — but say what we mean.
    const match = box.querySelector(".pmenu-match");
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
  await page.evaluate(() => window.__bemtvi.feed("<Esc><Esc>"));
  await sleep(150);

  // ---- Cmdline wildmenu docs float (a real [CmdlineDocs] doc-float window + wrap) ----
  // The wildmenu docs are no longer a `menu.docs` overlay — they render in a real
  // doc-float window, word-wrapped server-side to the box width. Read it from the frame
  // (the redraw the server produced) rather than the DOM: its lines are the wrapped rows.
  const longDesc =
    "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod " +
    "tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam";
  await page.evaluate((desc) => window.__bemtvi.execLua(
    "btv.cmdline_complete.setup {}\n" +
    "btv.user_command.create('Wrapcmd', function() end, { desc = [[" + desc + "]] })"), longDesc);
  await sleep(100);
  await page.evaluate(() => window.__bemtvi.feed(":Wrapcmd<Tab>"));
  await sleep(200);
  await page.evaluate(() => window.__bemtvi.feed("<Tab>")); // select row 0, arming docs
  await sleep(200);

  const docs = await page.evaluate(() => {
    const f = window.__bemtvi.frame() || {};
    const w = (f.windows || []).find((win) => win.file_name === "[CmdlineDocs]");
    if (!w) return null;
    const lines = w.lines || [];
    const widths = lines.map((l) => String(l).length);
    return { rowCount: lines.length, maxW: Math.max(...widths, 0) };
  });
  check("docs: the doc-float window paints", docs !== null, JSON.stringify(docs));
  if (docs) {
    check("docs: long desc wrapped onto multiple rows", docs.rowCount > 3, JSON.stringify(docs));
    check("docs: every row fits the box width (<=60)", docs.maxW > 0 && docs.maxW <= 60, JSON.stringify(docs));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
