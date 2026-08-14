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

  // ---- 1. Renderer is the DOM renderer, not a <pre> ----
  // The first frame paints on the redraw that follows `ready`, so poll briefly rather
  // than read the DOM the instant the Worker reports ready (which races the paint).
  let firstPaint = { noPre: false, hasRows: false };
  for (let i = 0; i < 60; i++) {
    firstPaint = await page.evaluate(() => ({
      noPre: document.querySelector("#grid pre") === null,
      hasRows: document.querySelectorAll("#grid .row").length > 0,
    }));
    if (firstPaint.noPre && firstPaint.hasRows) break;
    await sleep(50);
  }
  check("renderer: paints .row cells (DOM renderer, not a <pre>)",
    firstPaint.noPre && firstPaint.hasRows, JSON.stringify(firstPaint));

  // ---- 2. Command line sits BELOW the status line (bug #1) ----
  // Put text in, then read the y of the focused window's status line vs the command
  // line. The command line must be strictly below the status line (greater top px).
  await page.evaluate(() => window.__bemtvi.feed("ihello<Esc>"));
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
  await page.evaluate(() => window.__bemtvi.feed("0viw"));
  const selInfo = await page.evaluate(() => {
    const mode = window.__bemtvi.frame().mode_label;
    const selText = [...document.querySelectorAll("#grid .sel")].map((e) => e.textContent).join("");
    return { mode, selText };
  });
  check(
    "visual: `viw` enters VISUAL and paints the selection (.sel spans)",
    selInfo.mode === "VISUAL" && selInfo.selText.length >= 3 && "hello".startsWith(selInfo.selText),
    JSON.stringify(selInfo),
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // Mouse cell tests use absolute screen columns; drop the number gutter so a screen
  // column maps 1:1 to a buffer column (the default window has a 4-cell gutter).
  await page.evaluate(() => window.__bemtvi.feed(":set nonumber norelativenumber<CR>"));

  // ---- 4. Cursor renders as a shaped cell that tracks position (bug #3) ----
  // Normal mode → a block cursor cell exists; insert mode → a bar cursor.
  const normalCur = await page.evaluate(() => document.querySelectorAll("#grid .cur-block").length > 0);
  await page.evaluate(() => window.__bemtvi.feed("i"));
  const insertCur = await page.evaluate(() => ({
    mode: window.__bemtvi.frame().mode_label,
    bar: document.querySelectorAll("#grid .cur-bar").length > 0,
  }));
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  check("cursor: block cell in NORMAL, bar cell in INSERT (shape tracks mode)",
    normalCur && insertCur.mode === "INSERT" && insertCur.bar, JSON.stringify(insertCur));

  // ---- 5. Mouse click moves the cursor to the clicked cell (bug #2) ----
  // Buffer is "hello"; reset cursor to col 0, then click cell col 3 → cursor col 3.
  await page.evaluate(() => window.__bemtvi.feed("0"));
  await page.evaluate(() => window.__bemtvi.mouse("left", "press", "", 0, 3));
  await page.evaluate(() => window.__bemtvi.mouse("left", "release", "", 0, 3));
  const afterClick = await page.evaluate(() => window.__bemtvi.cursor());
  check("mouse: left click moves the cursor to the clicked cell",
    afterClick && afterClick.col === 3, JSON.stringify(afterClick));

  // ---- 6. Mouse drag selects a visual range (bug #2 + #3) ----
  await page.evaluate(() => window.__bemtvi.mouse("left", "press", "", 0, 0));
  await page.evaluate(() => window.__bemtvi.mouse("left", "drag", "", 0, 4));
  const dragMode = await page.evaluate(() => window.__bemtvi.frame().mode_label);
  const dragSel = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .sel")].map((e) => e.textContent).join(""));
  await page.evaluate(() => window.__bemtvi.mouse("left", "release", "", 0, 4));
  check("mouse: left drag enters VISUAL and paints a selection",
    dragMode === "VISUAL" && dragSel.length > 0, JSON.stringify({ dragMode, dragSel }));
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // ---- 6b. A click that wobbles WITHIN one cell must NOT enter Visual. Drives the
  // real DOM mousedown/mousemove/mouseup path (not the `mouse` hook, which bypasses
  // it): the browser fires `mousemove` per sub-cell pixel, so before the same-cell
  // coalescing every casual click / touchpad tap-and-move dropped into VISUAL.
  await page.evaluate(() => window.__bemtvi.feed("0")); // back to NORMAL, cursor col 0
  const geom = await page.evaluate(() => {
    const m = window.__bemtvi.cellMetrics();
    const r = document.getElementById("grid").getBoundingClientRect();
    return { cw: m.cw, ch: m.ch, left: r.left, top: r.top };
  });
  const cellCenter = (col, row) => ({
    x: geom.left + (col + 0.5) * geom.cw,
    y: geom.top + (row + 0.5) * geom.ch,
  });
  const press = cellCenter(2, 0);
  await page.mouse.move(press.x, press.y);
  await page.mouse.down();
  await page.mouse.move(press.x + geom.cw * 0.25, press.y); // sub-cell jitter, same cell
  await page.mouse.up();
  const jitterMode = await page.evaluate(() => window.__bemtvi.frame().mode_label);
  check("mouse: a within-cell jitter during a click stays in NORMAL (no spurious VISUAL)",
    jitterMode === "NORMAL", JSON.stringify({ jitterMode }));
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // ---- 6c. A real cross-cell drag still enters Visual — the coalescing only drops
  // within-cell noise, it must not break dragging out a selection.
  const from = cellCenter(0, 0), to = cellCenter(4, 0);
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(to.x, to.y);
  const realDragMode = await page.evaluate(() => window.__bemtvi.frame().mode_label);
  await page.mouse.up();
  check("mouse: a cross-cell DOM drag still enters VISUAL",
    realDragMode === "VISUAL", JSON.stringify({ realDragMode }));
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // ---- 6d. A focus-steal *between* mousedown and mouseup must release the drag latch.
  // In a real browser the OS clipboard-read permission chip (and alt-tab, alerts, …) can
  // grab focus mid-press, so the page never sees the `mouseup`. Without a `blur` reset the
  // left button stayed latched and every later move re-entered VISUAL (the "button is
  // stuck down" report). Simulate it: press, fire `blur` (no `mouseup` reaches the page),
  // then move to a new cell — it must stay in NORMAL, not drag out a selection.
  // Seed a known line and press a *distinct* cell from 6c's (0,0) press, so this is a fresh
  // single click — pressing the same cell again so soon reads as a double-click (word-select →
  // VISUAL) and would mask the latch behaviour we're checking.
  await page.evaluate(() => window.__bemtvi.feed("ggdGihello world<Esc>gg0"));
  const stuckFrom = cellCenter(6, 0), stuckTo = cellCenter(10, 0);
  await page.mouse.move(stuckFrom.x, stuckFrom.y);
  await page.mouse.down();
  await page.evaluate(() => window.dispatchEvent(new Event("blur"))); // focus stolen — mouseup lost
  await page.mouse.move(stuckTo.x, stuckTo.y);
  // The post-blur move is a no-op (the latch is released), so it queues no redraw of its own;
  // poll for the latch-release frame to settle. With the bug the move drags out a selection and
  // VISUAL latches *permanently*, so this stays VISUAL through the timeout → the check fails.
  let afterStealMode = "VISUAL";
  for (let i = 0; i < 25 && afterStealMode !== "NORMAL"; i++) {
    afterStealMode = await page.evaluate(() => window.__bemtvi.frame().mode_label);
    if (afterStealMode !== "NORMAL") await sleep(20);
  }
  await page.mouse.up();
  check("mouse: a focus-steal mid-press releases the drag (move stays NORMAL, no stuck VISUAL)",
    afterStealMode === "NORMAL", JSON.stringify({ afterStealMode }));
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // ---- 7. Mouse wheel scrolls the buffer (bug #2) ----
  // Fill enough lines to scroll (via real keystrokes — `100o` opens 100 lines), then a
  // wheel-down over the window must advance the top visible line.
  await page.evaluate(() => window.__bemtvi.feed("ggdG"));
  await page.evaluate(() => window.__bemtvi.feed("100oline<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed("gg"));
  const topBefore = await page.evaluate(() => window.__bemtvi.frame().windows.find((w) => w.focused).numbers[0]);
  for (let i = 0; i < 5; i++) await page.evaluate(() => window.__bemtvi.mouse("wheel", "down", "", 5, 5));
  await sleep(200);
  const topAfter = await page.evaluate(() => window.__bemtvi.frame().windows.find((w) => w.focused).numbers[0]);
  check("mouse: wheel-down scrolls the viewport (top line advances)",
    topAfter > topBefore, JSON.stringify({ topBefore, topAfter }));

  // ---- 8. Syntax highlighting colors a known token (the <pre> couldn't) ----
  // Open a Rust buffer with a keyword; tree-sitter highlighting must color `fn` (an
  // inline `color:` style on its span). The highlighter loads the grammar async.
  await page.evaluate(() => window.__bemtvi.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__bemtvi.feed("ggdGifn main() {}<Esc>"));
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

  // ---- 9. btv.ui.select opens a floating menu and confirms a choice ----
  // The widget runs entirely in the wasm edit-host (no server). Open a three-item
  // chooser, assert the bordered list paints with the labels, move + confirm, and
  // read back the choice the promise resolved to (btv.ui.select is promise-only).
  const luaResult = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);
  await page.evaluate(() => window.__bemtvi.execLua(
    "_G.picked = nil\n" +
    "btv.ui.select({ 'alpha', 'beta', 'gamma' }, {}):next(function(item)\n" +
    "  _G.picked = item\n" +
    "end)"));
  await sleep(100);
  const menuRows = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .pmenu .row")].map((e) => e.textContent.trim()));
  check("btv.ui.select: the menu paints its item labels",
    menuRows.length >= 3 && menuRows[0] === "alpha" && menuRows[1] === "beta" && menuRows[2] === "gamma",
    JSON.stringify(menuRows));

  // The menu opens NOSELECT: the first `j` reveals the highlight at row 0 (alpha), the
  // second moves it to row 1 (beta) — same as the native `select_default_keys_navigate_
  // and_confirm` test, which feeds "jj" for this reason. One `j` confirms alpha.
  await page.evaluate(() => window.__bemtvi.feed("jj")); // reveal at alpha, then -> beta
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  await sleep(100);
  // execLua returns a rendered `ok:<value>` string, so match on content (as the
  // other verify scripts do) rather than an exact JS value.
  const picked = String(await luaResult("return _G.picked"));
  check("btv.ui.select: <CR> confirms the highlighted row (promise resolves to the item)",
    /beta/.test(picked), JSON.stringify({ picked }));
  const menuGone = await page.evaluate(() => document.querySelectorAll("#grid .pmenu .row").length === 0);
  check("btv.ui.select: the menu closes after confirm", menuGone);

  // ---- 10. btv.picker: a fuzzy finder with a prompt, runs in the pure-wasm build ----
  // Register an in-memory static source (no process spawn, so it works serverless),
  // open it, and verify the prompt row, fuzzy filtering with match highlighting, and
  // confirm — the float-list widget's Phase-2 surface in the browser.
  await page.evaluate(() => window.__bemtvi.execLua(
    "_G.chosen = nil\n" +
    "btv.picker.source { name = 'demo',\n" +
    "  items = function(ctx)\n" +
    "    for _, c in ipairs({ 'crimson', 'cornflower', 'cerulean', 'magenta' }) do ctx.push { text = c } end\n" +
    "  end,\n" +
    "  confirm = function(item) _G.chosen = item.text end }\n" +
    "btv.picker.open('demo')"));
  await sleep(120);
  const pickerOpen = await page.evaluate(() => {
    // `:not(.pmenu-sep)` drops the rule a picker draws between the prompt and the list
    // (chrome the core reserves a row for), leaving only candidate rows.
    const all = [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt):not(.pmenu-sep)")]
      .map((e) => e.textContent.trim());
    return {
      rows: all.filter((t) => t !== ""),          // non-empty (fixed box pads empties)
      total: all.length,                          // the fixed box height (> item count)
      prompt: document.querySelector("#grid .pmenu .pmenu-prompt") !== null,
    };
  });
  check("btv.picker: opens a prompt + the streamed candidate rows in a fixed box",
    pickerOpen.prompt && pickerOpen.rows.length === 4 && pickerOpen.rows[0] === "crimson" && pickerOpen.total > 4,
    JSON.stringify(pickerOpen));

  // Type a subsequence unique to "cerulean": the prompt grabs the keys, the matcher
  // narrows the list, and the matched characters carry the .pmenu-match class.
  await page.evaluate(() => window.__bemtvi.feed("ceru"));
  await sleep(120);
  const filtered = await page.evaluate(() => ({
    query: window.__bemtvi.frame().menu?.query,
    rows: [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt):not(.pmenu-sep)")]
      .map((e) => e.textContent.trim()).filter((t) => t !== ""),
    matched: [...document.querySelectorAll("#grid .pmenu .pmenu-match")].length,
  }));
  check("btv.picker: typing filters the list and highlights matched chars",
    filtered.query === "ceru" && filtered.rows.length === 1 && filtered.rows[0] === "cerulean" && filtered.matched > 0,
    JSON.stringify(filtered));

  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  await sleep(100);
  const chosen = String(await luaResult("return _G.chosen"));
  check("btv.picker: <CR> confirms the highlighted item", /cerulean/.test(chosen), chosen);

  // ---- 11. btv.complete: trigger-char async source completes in the pure-wasm build ----
  // A `trigger = { chars = { ':' } }` emoji source (Phase 4-E) registered serverless:
  // it streams candidates off the input path (the async substrate the picker shares),
  // and the engine wakes it only after a ':'. Proves trigger-char gating + the async
  // completion path run in the browser, and that accept folds the ':' into the prefix.
  await page.evaluate(() => window.__bemtvi.execLua(
    "btv.complete.source {\n" +
    "  name = 'emoji', debounce = 0, trigger = { chars = { ':' } },\n" +
    "  complete = function(ctx)\n" +
    "    for _, e in ipairs({ { ':smile:', 'SMILE' }, { ':rocket:', 'ROCKET' } }) do\n" +
    "      if e[1]:sub(1, #ctx.prefix) == ctx.prefix then ctx.push { text = e[1], insert = e[2] } end\n" +
    "    end\n" +
    "  end }\n" +
    "btv.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'emoji' } } }"));
  // Fresh line, then a trigger-char prefix. `Esc` first to leave any prior mode.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>o:sm"));
  await sleep(120);
  const completeOpen = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt)")]
      .map((e) => e.textContent.trim()).filter((t) => t !== ""));
  check("btv.complete: a trigger-char async source wakes after ':' and streams its row",
    completeOpen.length === 1 && completeOpen[0] === ":smile:", JSON.stringify(completeOpen));

  // Navigate to the row and accept: the ':' was folded into the prefix, so `:sm` is
  // replaced by the emoji's insert text.
  await page.evaluate(() => window.__bemtvi.feed("<C-n><C-y>"));
  await sleep(100);
  const completedLine = String(await luaResult("return btv.current_line()"));
  // The insert text is `SMILE` (uppercase) — distinct from the `:smile:` label, so a
  // match proves the emoji's `insert` replaced the prefix, not the label being left.
  check("btv.complete: <C-y> accepts, replacing the whole ':sm' from the trigger char",
    /SMILE/.test(completedLine) && !/smile/.test(completedLine), JSON.stringify(completedLine));

  // ---- 11b. btv.complete MOUSE: a click on a popup row accepts it (overlay mouse) ----
  // The popup is non-grabbing and the browser forwards a raw screen cell; core
  // hit-tests it back to the row (no client-side geometry). A leading space anchors
  // the box off column 0, then clicking the row highlights it and a second click
  // accepts it — the same select-then-accept as <C-n> then <C-y>, by pointer.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>o :sm"));
  await sleep(120);
  const cmenu = await page.evaluate(() => {
    const m = window.__bemtvi.frame().menu;
    return m ? { row: m.row, col: m.col, items: (m.items || []).length } : null;
  });
  check("btv.complete mouse: a one-row popup is open under the caret",
    cmenu && cmenu.items === 1, JSON.stringify(cmenu));
  // Completion omits its top border, so the first list row is on the box's top row;
  // nonumber makes the text-area column a global cell (clamped off col 0).
  const clickCol = Math.max(cmenu.col, 1);
  await page.evaluate(([r, c]) => window.__bemtvi.mouse("left", "press", "", r, c), [cmenu.row, clickCol]);
  await sleep(60);
  const afterFirst = await page.evaluate(() => window.__bemtvi.frame().menu?.selected_active);
  check("btv.complete mouse: the first click highlights the row", afterFirst === true, JSON.stringify({ afterFirst }));
  await page.evaluate(([r, c]) => window.__bemtvi.mouse("left", "press", "", r, c), [cmenu.row, clickCol]);
  await sleep(100);
  const clickedLine = String(await luaResult("return btv.current_line()"));
  check("btv.complete mouse: a second click on the row accepts it (line gets the insert text)",
    /SMILE/.test(clickedLine), JSON.stringify(clickedLine));

  // ---- 12. cmdline wildmenu: float geometry + order + back-cycle ----
  // Three layout bugs hit the web client and not the cell-grid (GUI/TUI) ones, and the
  // textContent-only checks above couldn't catch them: the rows collapsed to the box
  // top (a CSS specificity clash made `.pmenu .row` absolute), the float hovered a
  // status line above the command line instead of kissing it, and the list wasn't
  // reversed so the best match sat farthest from the input. We also drive REAL typed
  // keys here (through the key encoder, not the `feed` hook) so Shift+Tab exercises the
  // encoding path — it was dropping the Shift and sending a plain forward `<Tab>`.
  const wildSnap = () => page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    const cmd = document.querySelector(".cmdline");
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")].map((r) => ({
      text: r.textContent.replace(/\s+$/, ""),
      top: Math.round(r.getBoundingClientRect().top),
    }));
    return {
      selected: window.__bemtvi.frame().menu?.selected,
      boxBottom: Math.round(box.getBoundingClientRect().bottom),
      cmdTop: cmd ? Math.round(cmd.getBoundingClientRect().top) : null,
      rows,
    };
  });
  await page.evaluate(() => window.__bemtvi.execLua("btv.cmdline_complete.setup {}"));
  await page.evaluate(() => { window.__bemtvi.feed("<Esc>"); document.getElementById("kbd").focus(); });
  await page.keyboard.type(":tab");
  await page.keyboard.press("Tab"); // open the wildmenu + highlight the top fuzzy match
  // The command catalog resolves through an async Lua source; poll for the full list.
  let wild = null;
  for (let i = 0; i < 40; i++) {
    wild = await wildSnap();
    if (wild && wild.rows.length >= 3 && wild.rows.some((r) => r.text === "tabnew")) break;
    await sleep(50);
  }
  const { ch } = await page.evaluate(() => window.__bemtvi.cellMetrics());

  // Rows fill the float: each on its own line, one cell-height apart, none overlapping
  // (the collapse bug stacked every row at the same `top`).
  const tops = (wild?.rows || []).map((r) => r.top);
  const stacked = tops.length >= 3
    && new Set(tops).size === tops.length
    && tops.every((t, i) => i === 0 || Math.abs(t - tops[i - 1] - ch) <= 1);
  check("cmdline wildmenu: rows stack down the float (not collapsed to the top)",
    !!wild && stacked, JSON.stringify(tops));

  // Reversed: the best fuzzy match (`tabnew`, the server's row 0) sits at the BOTTOM
  // nearest the command line; the worst (`tab`) at the top.
  const topRow = wild?.rows[0]?.text;
  const bottomRow = wild?.rows.at(-1)?.text;
  check("cmdline wildmenu: list is reversed (best match at the bottom, nearest input)",
    topRow === "tab" && bottomRow === "tabnew", JSON.stringify({ topRow, bottomRow }));

  // Kisses the command line: the box's bottom edge meets the command-line row (within a
  // border width) instead of floating a status line above it.
  check("cmdline wildmenu: the float bottom kisses the command line",
    !!wild && wild.cmdTop != null && Math.abs(wild.boxBottom - wild.cmdTop) <= 3,
    JSON.stringify({ boxBottom: wild?.boxBottom, cmdTop: wild?.cmdTop }));

  // Shift+Tab cycles BACKWARD: the first <Tab> selected row 0, so a real Shift+Tab — a
  // special key the encoder must tag `<S-Tab>`, not a bare `<Tab>` — wraps to the last.
  const selBefore = wild?.selected;
  await page.keyboard.down("Shift");
  await page.keyboard.press("Tab");
  await page.keyboard.up("Shift");
  let wild2 = null;
  for (let i = 0; i < 30; i++) {
    wild2 = await wildSnap();
    if (wild2 && wild2.selected !== selBefore) break;
    await sleep(50);
  }
  check("cmdline wildmenu: Shift+Tab cycles the selection backward (S-Tab encoded, not Tab)",
    selBefore === 0 && wild2?.selected === (wild?.rows.length ?? 0) - 1,
    JSON.stringify({ selBefore, after: wild2?.selected, total: wild?.rows.length }));
  await page.keyboard.press("Escape");
  await page.keyboard.press("Escape"); // close the wildmenu + command line

  // ---- Wide characters occupy two display columns (CJK / emoji) ----
  // The server counts a wide glyph as two columns (unicode-width); the DOM renderer
  // must too. With a one-column-per-codepoint model the cursor and every glyph after a
  // wide char shifted left by one and the row stopped aligning with an all-ASCII row.
  // Put a CJK char mid-line, land the cursor on the ASCII char after it, and confirm
  // the reverse-video cur-block sits on that exact glyph (it landed on the next one,
  // "d", before the fix).
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:enew!<CR>iab你cd<Esc>0fc"));
  const wideAfter = await page.evaluate(() => {
    const cur = document.querySelector("#grid .cur-block");
    return { curText: cur ? cur.textContent : null };
  });
  check("wide char: cursor lands on the ASCII glyph after a CJK char (two-column width)",
    wideAfter.curText === "c", JSON.stringify(wideAfter));

  // Cursor ON the wide glyph is a single reverse-video cell holding just that glyph —
  // the continuation column renders nothing, so there is exactly one cur-block. Its
  // rendered box must be exactly two ASCII cells wide (the glyph sits in a fixed
  // inline-block box), or the glyph and the text after it drift out of column. Measure
  // the wide cur-block, then the cur-block over an ASCII glyph, and compare.
  await page.evaluate(() => window.__bemtvi.feed("0ll"));
  const wideBox = await page.evaluate(() => {
    const blocks = [...document.querySelectorAll("#grid .cur-block")];
    return { count: blocks.length, text: blocks.map((b) => b.textContent).join(""),
      w: blocks[0] ? blocks[0].getBoundingClientRect().width : null };
  });
  await page.evaluate(() => window.__bemtvi.feed("0"));
  const asciiBox = await page.evaluate(() => {
    const b = document.querySelector("#grid .cur-block");
    return { text: b ? b.textContent : null, w: b ? b.getBoundingClientRect().width : null };
  });
  check("wide char: cursor on the CJK glyph is a single block cell holding the glyph",
    wideBox.count === 1 && wideBox.text === "你", JSON.stringify(wideBox));
  // Within 1px to allow sub-pixel rounding of the half-cell-derived box width.
  check("wide char: the CJK glyph box is exactly two ASCII cells wide (no visual drift)",
    wideBox.w != null && asciiBox.w != null && Math.abs(wideBox.w - 2 * asciiBox.w) <= 1,
    JSON.stringify({ wide: wideBox.w, ascii: asciiBox.w }));

  // ---- Single-column symbols / Nerd-Font glyphs are pinned to ONE cell ----
  // unicode-width counts a box-drawing / arrow / Private-Use (Nerd Font) glyph as ONE
  // column, but a fallback font often draws it wider, dragging the rest of the line off
  // the grid. The renderer boxes these (U+2190+) to a single cell so the advance stays
  // exactly one ASCII cell. Use U+2500 (box drawing) — a real Unicode symbol that needs
  // no special font. Put it mid-line, land the cursor on it, and measure its box.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:enew!<CR>iab─cd<Esc>0fc"));
  const symAfter = await page.evaluate(() => {
    const cur = document.querySelector("#grid .cur-block");
    return { curText: cur ? cur.textContent : null };
  });
  check("symbol: a one-column symbol keeps the next glyph on its column",
    symAfter.curText === "c", JSON.stringify(symAfter));
  await page.evaluate(() => window.__bemtvi.feed("0ll"));   // onto the U+2500
  const symBox = await page.evaluate(() => {
    const b = document.querySelector("#grid .cur-block");
    return { text: b ? b.textContent : null, w: b ? b.getBoundingClientRect().width : null };
  });
  check("symbol: the one-column symbol box is exactly one ASCII cell wide",
    symBox.text === "─" && symBox.w != null && asciiBox.w != null
      && Math.abs(symBox.w - asciiBox.w) <= 1,
    JSON.stringify({ sym: symBox.w, ascii: asciiBox.w }));

  // ---- Right-hugging Nerd separators are anchored to the cell's right edge ----
  // A left-pointing powerline separator () is shaped with its ink parked at the right
  // of an advance box wider than its cell; left-anchoring it draws over the next cell.
  // The renderer measures the ink (Canvas) and, when it hugs the right, pins the ink's
  // right edge to the box via a negative text-indent (a right-pointing  hugs the left
  // and keeps the natural anchor). This stays self-consistent whether or not a Nerd
  // font is installed: with no Nerd font both glyphs fall back to a cell-width box with
  // centred ink (no hug → no indent), and the assertion still holds.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:enew!<CR>i\u{e0b0}\u{e0b2}<Esc>"));
  await sleep(150);
  const seps = await page.evaluate(() => {
    const grid = document.getElementById("grid");
    const ctx = document.createElement("canvas").getContext("2d");
    ctx.textAlign = "left"; ctx.textBaseline = "alphabetic";
    ctx.font = `15px ${getComputedStyle(grid).fontFamily}`;
    const measure = (cp) => {
      const m = ctx.measureText(String.fromCodePoint(cp));
      const inkLeft = -m.actualBoundingBoxLeft, inkRight = m.actualBoundingBoxRight;
      return { hugRight: inkRight > inkLeft && (m.width - inkRight) < inkLeft, inkRight };
    };
    const indentOf = (chStr) => {
      const span = [...document.querySelectorAll("#grid .win .row span")].find((s) => s.textContent === chStr);
      return span ? parseFloat(getComputedStyle(span).textIndent) || 0 : null;
    };
    return {
      right: { ...measure(0xe0b0), indent: indentOf("\u{e0b0}") },
      left: { ...measure(0xe0b2), indent: indentOf("\u{e0b2}") },
    };
  });
  // Right-pointing: hugs left → no indent. Left-pointing: hugs right → negative indent.
  const okRight = seps.right.indent != null && Math.abs(seps.right.indent) < 0.5 && !seps.right.hugRight;
  const okLeft = seps.left.hugRight
    ? (seps.left.indent != null && seps.left.indent < -0.5)   // Nerd font present: pinned right
    : (seps.left.indent != null && Math.abs(seps.left.indent) < 0.5); // no Nerd font: no hug, no indent
  check("nerd separator: right-hugging glyph anchored to the cell's right edge (left-pointing), left-hugging unshifted",
    okRight && okLeft, JSON.stringify(seps));

  // ---- A wide glyph is never re-anchored ----
  // The ink-shift only applies to single-column glyphs; a kanji / emoji sits naturally in
  // its two-cell box with no text-indent. (Regression: the shift used to run on wide glyphs
  // too, and a font that shapes a kanji / emoji right-biased then dragged it off-grid.)
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:enew!<CR>i\u{e0b2}你A\u{e0b2}😀<Esc>"));
  await sleep(150);
  const wideIndents = await page.evaluate(() => {
    const indentOf = (chStr) => {
      const span = [...document.querySelectorAll("#grid .win .row span")].find((s) => s.textContent === chStr);
      return span ? parseFloat(getComputedStyle(span).textIndent) || 0 : null;
    };
    return { kanji: indentOf("你"), emoji: indentOf("\u{1f600}") };
  });
  check("wide glyph: a kanji / emoji keeps a zero indent (ink-shift gated to single-column glyphs)",
    wideIndents.kanji != null && Math.abs(wideIndents.kanji) < 0.5
      && wideIndents.emoji != null && Math.abs(wideIndents.emoji) < 0.5,
    JSON.stringify(wideIndents));

  // ---- The 'statusline' option drives the rendered status bar ----
  // The server renders the `%`-format (or its built-in default) into styled segments on
  // the wasm build too; the client must paint those rather than synthesize its own bar,
  // or `:set statusline` is silently ignored. Set a sentinel value and read the bar text.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:set statusline=BTVSTATUS<CR>"));
  await sleep(200);
  const slSet = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .statusline")].map((e) => e.textContent).join("|"));
  check("statusline: ':set statusline=...' renders into the status bar", slSet.includes("BTVSTATUS"), slSet);
  // ...and the Lua surface (vim.o.statusline) drives it too.
  await page.evaluate(() => window.__bemtvi.feed(":lua vim.o.statusline='BTVLUA'<CR>"));
  await sleep(200);
  const slLua = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .statusline")].map((e) => e.textContent).join("|"));
  check("statusline: 'vim.o.statusline' drives the status bar", slLua.includes("BTVLUA"), slLua);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — DOM renderer + mouse + selection/cursor + layout + highlighting + wildmenu + wide chars + statusline verified in a real browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
