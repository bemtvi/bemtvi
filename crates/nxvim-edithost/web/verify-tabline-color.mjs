// Focused verifier for tabline CHROME colors (TabLine / TabLineSel / TabLineFill)
// in the web client: a colorscheme that defines those groups must color the
// built-in tabline — the bar fill, the inactive tabs, and the active tab — not
// fall back to the StatusLine colors. Drives the real wasm edit-host in headless
// Chromium.
//
//   node verify-tabline-color.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8098;

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// The three test colors, as the rgb() string getComputedStyle returns.
const FILL = "rgb(24, 24, 32)";      // #181820  TabLineFill bg
const TAB = "rgb(48, 48, 64)";       // #303040  TabLine bg
const SEL = "rgb(192, 160, 255)";    // #c0a0ff  TabLineSel bg

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

  // Define the tabline chrome groups, then open a second tab so the tabline shows
  // (the new tab is the active one).
  await page.evaluate(() => window.__nxvim.execLua(
    "vim.api.nvim_set_hl(0, 'TabLine',     { fg = '#a0a0a0', bg = '#303040' })\n" +
    "vim.api.nvim_set_hl(0, 'TabLineSel',  { fg = '#101010', bg = '#c0a0ff' })\n" +
    "vim.api.nvim_set_hl(0, 'TabLineFill', { fg = '#606060', bg = '#181820' })\n" +
    "vim.cmd('tabnew')"));
  await sleep(200);

  const tl = await page.evaluate(() => {
    const rows = [...document.querySelectorAll("#grid > div.row.statusline")];
    const bar = rows.find((r) => r.style.top === "0px");
    if (!bar) return null;
    const spans = [...bar.querySelectorAll("span")];
    return {
      barBg: getComputedStyle(bar).backgroundColor,
      spanCount: spans.length,
      spanBgs: spans.map((s) => getComputedStyle(s).backgroundColor),
    };
  });
  check("tabline: bar paints", tl !== null, JSON.stringify(tl));
  if (tl) {
    check("tabline: bar fill uses TabLineFill bg", tl.barBg === FILL, JSON.stringify(tl));
    check("tabline: two tab cells", tl.spanCount === 2, JSON.stringify(tl));
    // tab 1 (inactive) → TabLine, tab 2 (active, just created) → TabLineSel.
    check("tabline: inactive tab uses TabLine bg", tl.spanBgs[0] === TAB, JSON.stringify(tl));
    check("tabline: active tab uses TabLineSel bg", tl.spanBgs[1] === SEL, JSON.stringify(tl));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
