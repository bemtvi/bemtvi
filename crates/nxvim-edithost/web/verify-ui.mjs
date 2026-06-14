// Playwright verifier for the DOM renderer + mouse port (the issues fixed: command
// line over the status line, no mouse events, cursor/visual mode not painting, plain
// <pre> with no syntax highlighting). Drives the real wasm edit-host in headless
// Chromium and asserts the rendered DOM — selection spans, cursor classes, the
// command-line/status-line layout, mouse click/drag/wheel, and tree-sitter highlight
// colors. Companion to verify.mjs (which covers the editor/transport/OPFS contract).
//
//   node verify-ui.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8097;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
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

  // ---- 1. Renderer is the DOM renderer, not a <pre> ----
  const noPre = await page.evaluate(() => document.querySelector("#grid pre") === null);
  const hasRows = await page.evaluate(() => document.querySelectorAll("#grid .row").length > 0);
  check("renderer: paints .row cells (DOM renderer, not a <pre>)", noPre && hasRows);

  // ---- 2. Command line sits BELOW the status line (bug #1) ----
  // Put text in, then read the y of the focused window's status line vs the command
  // line. The command line must be strictly below the status line (greater top px).
  await page.evaluate(() => window.__nxvim.feed("ihello<Esc>"));
  const layout = await page.evaluate(() => {
    const status = document.querySelector(".win .statusline");
    const cmd = document.querySelector(".cmdline");
    return {
      statusTop: status ? Math.round(status.getBoundingClientRect().top) : null,
      cmdTop: cmd ? Math.round(cmd.getBoundingClientRect().top) : null,
    };
  });
  check(
    "layout: command line is below the status line (not overlapping)",
    layout.statusTop != null && layout.cmdTop != null && layout.cmdTop > layout.statusTop,
    JSON.stringify(layout),
  );

  // ---- 3. Visual-mode selection paints .sel cells (bug #3) ----
  // Select the whole word with `viw`; the selected glyphs carry the .sel class (the
  // cursor cell on the last char is a reverse-video cur-block, so .sel is the prefix).
  await page.evaluate(() => window.__nxvim.feed("0viw"));
  const selInfo = await page.evaluate(() => {
    const mode = window.__nxvim.frame().mode_label;
    const selText = [...document.querySelectorAll("#grid .sel")].map((e) => e.textContent).join("");
    return { mode, selText };
  });
  check(
    "visual: `viw` enters VISUAL and paints the selection (.sel spans)",
    selInfo.mode === "VISUAL" && selInfo.selText.length >= 3 && "hello".startsWith(selInfo.selText),
    JSON.stringify(selInfo),
  );
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // Mouse cell tests use absolute screen columns; drop the number gutter so a screen
  // column maps 1:1 to a buffer column (the default window has a 4-cell gutter).
  await page.evaluate(() => window.__nxvim.feed(":set nonumber norelativenumber<CR>"));

  // ---- 4. Cursor renders as a shaped cell that tracks position (bug #3) ----
  // Normal mode → a block cursor cell exists; insert mode → a bar cursor.
  const normalCur = await page.evaluate(() => document.querySelectorAll("#grid .cur-block").length > 0);
  await page.evaluate(() => window.__nxvim.feed("i"));
  const insertCur = await page.evaluate(() => ({
    mode: window.__nxvim.frame().mode_label,
    bar: document.querySelectorAll("#grid .cur-bar").length > 0,
  }));
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  check("cursor: block cell in NORMAL, bar cell in INSERT (shape tracks mode)",
    normalCur && insertCur.mode === "INSERT" && insertCur.bar, JSON.stringify(insertCur));

  // ---- 5. Mouse click moves the cursor to the clicked cell (bug #2) ----
  // Buffer is "hello"; reset cursor to col 0, then click cell col 3 → cursor col 3.
  await page.evaluate(() => window.__nxvim.feed("0"));
  await page.evaluate(() => window.__nxvim.mouse("left", "press", "", 0, 3));
  await page.evaluate(() => window.__nxvim.mouse("left", "release", "", 0, 3));
  const afterClick = await page.evaluate(() => window.__nxvim.cursor());
  check("mouse: left click moves the cursor to the clicked cell",
    afterClick && afterClick.col === 3, JSON.stringify(afterClick));

  // ---- 6. Mouse drag selects a visual range (bug #2 + #3) ----
  await page.evaluate(() => window.__nxvim.mouse("left", "press", "", 0, 0));
  await page.evaluate(() => window.__nxvim.mouse("left", "drag", "", 0, 4));
  const dragMode = await page.evaluate(() => window.__nxvim.frame().mode_label);
  const dragSel = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .sel")].map((e) => e.textContent).join(""));
  await page.evaluate(() => window.__nxvim.mouse("left", "release", "", 0, 4));
  check("mouse: left drag enters VISUAL and paints a selection",
    dragMode === "VISUAL" && dragSel.length > 0, JSON.stringify({ dragMode, dragSel }));
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // ---- 7. Mouse wheel scrolls the buffer (bug #2) ----
  // Fill enough lines to scroll (via real keystrokes — `100o` opens 100 lines), then a
  // wheel-down over the window must advance the top visible line.
  await page.evaluate(() => window.__nxvim.feed("ggdG"));
  await page.evaluate(() => window.__nxvim.feed("100oline<Esc>"));
  await page.evaluate(() => window.__nxvim.feed("gg"));
  const topBefore = await page.evaluate(() => window.__nxvim.frame().windows.find((w) => w.focused).numbers[0]);
  for (let i = 0; i < 5; i++) await page.evaluate(() => window.__nxvim.mouse("wheel", "down", "", 5, 5));
  await sleep(200);
  const topAfter = await page.evaluate(() => window.__nxvim.frame().windows.find((w) => w.focused).numbers[0]);
  check("mouse: wheel-down scrolls the viewport (top line advances)",
    topAfter > topBefore, JSON.stringify({ topBefore, topAfter }));

  // ---- 8. Syntax highlighting colors a known token (the <pre> couldn't) ----
  // Open a Rust buffer with a keyword; tree-sitter highlighting must color `fn` (an
  // inline `color:` style on its span). The highlighter loads the grammar async.
  await page.evaluate(() => window.__nxvim.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__nxvim.feed("ggdGifn main() {}<Esc>"));
  let colored = false, detail = "";
  for (let i = 0; i < 40; i++) { // poll up to ~4s for the grammar to load + repaint
    const r = await page.evaluate(() => {
      const spans = [...document.querySelectorAll("#grid .win .row span[style]")];
      const styled = spans.filter((s) => /color\s*:/.test(s.getAttribute("style")));
      return { any: styled.length, sample: styled.slice(0, 4).map((s) => [s.textContent, s.getAttribute("style")]) };
    });
    if (r.any > 0) { colored = true; detail = JSON.stringify(r.sample); break; }
    detail = JSON.stringify(r);
    await sleep(100);
  }
  check("highlight: tree-sitter colors Rust tokens (inline color styles present)", colored, detail);

  // ---- 9. nx.ui.select opens a floating menu and confirms a choice ----
  // The widget runs entirely in the wasm edit-host (no server). Open a three-item
  // chooser, assert the bordered list paints with the labels, move + confirm, and
  // read back the choice the callback captured.
  const luaResult = (code) => page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);
  await page.evaluate(() => window.__nxvim.execLua(
    "_G.picked, _G.pickedIdx = nil, nil\n" +
    "nx.ui.select({ 'alpha', 'beta', 'gamma' }, {}, function(item, idx)\n" +
    "  _G.picked, _G.pickedIdx = item, idx\n" +
    "end)"));
  await sleep(100);
  const menuRows = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .pmenu .row")].map((e) => e.textContent.trim()));
  check("nx.ui.select: the menu paints its item labels",
    menuRows.length >= 3 && menuRows[0] === "alpha" && menuRows[1] === "beta" && menuRows[2] === "gamma",
    JSON.stringify(menuRows));

  await page.evaluate(() => window.__nxvim.feed("j")); // alpha -> beta
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  await sleep(100);
  // execLua returns a rendered `ok:<value>` string, so match on content (as the
  // other verify scripts do) rather than an exact JS value.
  const picked = String(await luaResult("return _G.picked"));
  const pickedIdx = String(await luaResult("return _G.pickedIdx"));
  check("nx.ui.select: <CR> confirms the highlighted row (item + 1-based index)",
    /beta/.test(picked) && /\b2\b/.test(pickedIdx), JSON.stringify({ picked, pickedIdx }));
  const menuGone = await page.evaluate(() => document.querySelectorAll("#grid .pmenu .row").length === 0);
  check("nx.ui.select: the menu closes after confirm", menuGone);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — DOM renderer + mouse + selection/cursor + layout + highlighting verified in a real browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
