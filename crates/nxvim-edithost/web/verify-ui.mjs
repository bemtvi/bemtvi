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

  // ---- 6b. A click that wobbles WITHIN one cell must NOT enter Visual. Drives the
  // real DOM mousedown/mousemove/mouseup path (not the `mouse` hook, which bypasses
  // it): the browser fires `mousemove` per sub-cell pixel, so before the same-cell
  // coalescing every casual click / touchpad tap-and-move dropped into VISUAL.
  await page.evaluate(() => window.__nxvim.feed("0")); // back to NORMAL, cursor col 0
  const geom = await page.evaluate(() => {
    const m = window.__nxvim.cellMetrics();
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
  const jitterMode = await page.evaluate(() => window.__nxvim.frame().mode_label);
  check("mouse: a within-cell jitter during a click stays in NORMAL (no spurious VISUAL)",
    jitterMode === "NORMAL", JSON.stringify({ jitterMode }));
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // ---- 6c. A real cross-cell drag still enters Visual — the coalescing only drops
  // within-cell noise, it must not break dragging out a selection.
  const from = cellCenter(0, 0), to = cellCenter(4, 0);
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(to.x, to.y);
  const realDragMode = await page.evaluate(() => window.__nxvim.frame().mode_label);
  await page.mouse.up();
  check("mouse: a cross-cell DOM drag still enters VISUAL",
    realDragMode === "VISUAL", JSON.stringify({ realDragMode }));
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // ---- 6d. A focus-steal *between* mousedown and mouseup must release the drag latch.
  // In a real browser the OS clipboard-read permission chip (and alt-tab, alerts, …) can
  // grab focus mid-press, so the page never sees the `mouseup`. Without a `blur` reset the
  // left button stayed latched and every later move re-entered VISUAL (the "button is
  // stuck down" report). Simulate it: press, fire `blur` (no `mouseup` reaches the page),
  // then move to a new cell — it must stay in NORMAL, not drag out a selection.
  // Seed a known line and press a *distinct* cell from 6c's (0,0) press, so this is a fresh
  // single click — pressing the same cell again so soon reads as a double-click (word-select →
  // VISUAL) and would mask the latch behaviour we're checking.
  await page.evaluate(() => window.__nxvim.feed("ggdGihello world<Esc>gg0"));
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
    afterStealMode = await page.evaluate(() => window.__nxvim.frame().mode_label);
    if (afterStealMode !== "NORMAL") await sleep(20);
  }
  await page.mouse.up();
  check("mouse: a focus-steal mid-press releases the drag (move stays NORMAL, no stuck VISUAL)",
    afterStealMode === "NORMAL", JSON.stringify({ afterStealMode }));
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
  // read back the choice the promise resolved to (nx.ui.select is promise-only).
  const luaResult = (code) => page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);
  await page.evaluate(() => window.__nxvim.execLua(
    "_G.picked = nil\n" +
    "nx.ui.select({ 'alpha', 'beta', 'gamma' }, {}):next(function(item)\n" +
    "  _G.picked = item\n" +
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
  check("nx.ui.select: <CR> confirms the highlighted row (promise resolves to the item)",
    /beta/.test(picked), JSON.stringify({ picked }));
  const menuGone = await page.evaluate(() => document.querySelectorAll("#grid .pmenu .row").length === 0);
  check("nx.ui.select: the menu closes after confirm", menuGone);

  // ---- 10. nx.picker: a fuzzy finder with a prompt, runs in the pure-wasm build ----
  // Register an in-memory static source (no process spawn, so it works serverless),
  // open it, and verify the prompt row, fuzzy filtering with match highlighting, and
  // confirm — the float-list widget's Phase-2 surface in the browser.
  await page.evaluate(() => window.__nxvim.execLua(
    "_G.chosen = nil\n" +
    "nx.picker.source { name = 'demo',\n" +
    "  items = function(ctx)\n" +
    "    for _, c in ipairs({ 'crimson', 'cornflower', 'cerulean', 'magenta' }) do ctx.push { text = c } end\n" +
    "  end,\n" +
    "  confirm = function(item) _G.chosen = item.text end }\n" +
    "nx.picker.open('demo')"));
  await sleep(120);
  const pickerOpen = await page.evaluate(() => {
    const all = [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt)")].map((e) => e.textContent.trim());
    return {
      rows: all.filter((t) => t !== ""),          // non-empty (fixed box pads empties)
      total: all.length,                          // the fixed box height (> item count)
      prompt: document.querySelector("#grid .pmenu .pmenu-prompt") !== null,
    };
  });
  check("nx.picker: opens a prompt + the streamed candidate rows in a fixed box",
    pickerOpen.prompt && pickerOpen.rows.length === 4 && pickerOpen.rows[0] === "crimson" && pickerOpen.total > 4,
    JSON.stringify(pickerOpen));

  // Type a subsequence unique to "cerulean": the prompt grabs the keys, the matcher
  // narrows the list, and the matched characters carry the .pmenu-match class.
  await page.evaluate(() => window.__nxvim.feed("ceru"));
  await sleep(120);
  const filtered = await page.evaluate(() => ({
    query: window.__nxvim.frame().menu?.query,
    rows: [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt)")]
      .map((e) => e.textContent.trim()).filter((t) => t !== ""),
    matched: [...document.querySelectorAll("#grid .pmenu .pmenu-match")].length,
  }));
  check("nx.picker: typing filters the list and highlights matched chars",
    filtered.query === "ceru" && filtered.rows.length === 1 && filtered.rows[0] === "cerulean" && filtered.matched > 0,
    JSON.stringify(filtered));

  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  await sleep(100);
  const chosen = String(await luaResult("return _G.chosen"));
  check("nx.picker: <CR> confirms the highlighted item", /cerulean/.test(chosen), chosen);

  // ---- 11. nx.complete: trigger-char async source completes in the pure-wasm build ----
  // A `trigger = { chars = { ':' } }` emoji source (Phase 4-E) registered serverless:
  // it streams candidates off the input path (the async substrate the picker shares),
  // and the engine wakes it only after a ':'. Proves trigger-char gating + the async
  // completion path run in the browser, and that accept folds the ':' into the prefix.
  await page.evaluate(() => window.__nxvim.execLua(
    "nx.complete.source {\n" +
    "  name = 'emoji', debounce = 0, trigger = { chars = { ':' } },\n" +
    "  complete = function(ctx, push, done)\n" +
    "    for _, e in ipairs({ { ':smile:', 'SMILE' }, { ':rocket:', 'ROCKET' } }) do\n" +
    "      if e[1]:sub(1, #ctx.prefix) == ctx.prefix then push { text = e[1], insert = e[2] } end\n" +
    "    end\n" +
    "    done()\n" +
    "  end }\n" +
    "nx.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'emoji' } } }"));
  // Fresh line, then a trigger-char prefix. `Esc` first to leave any prior mode.
  await page.evaluate(() => window.__nxvim.feed("<Esc>o:sm"));
  await sleep(120);
  const completeOpen = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .pmenu .row:not(.pmenu-prompt)")]
      .map((e) => e.textContent.trim()).filter((t) => t !== ""));
  check("nx.complete: a trigger-char async source wakes after ':' and streams its row",
    completeOpen.length === 1 && completeOpen[0] === ":smile:", JSON.stringify(completeOpen));

  // Navigate to the row and accept: the ':' was folded into the prefix, so `:sm` is
  // replaced by the emoji's insert text.
  await page.evaluate(() => window.__nxvim.feed("<C-n><C-y>"));
  await sleep(100);
  const completedLine = String(await luaResult("return nx.current_line()"));
  // The insert text is `SMILE` (uppercase) — distinct from the `:smile:` label, so a
  // match proves the emoji's `insert` replaced the prefix, not the label being left.
  check("nx.complete: <C-y> accepts, replacing the whole ':sm' from the trigger char",
    /SMILE/.test(completedLine) && !/smile/.test(completedLine), JSON.stringify(completedLine));

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
      selected: window.__nxvim.frame().menu?.selected,
      boxBottom: Math.round(box.getBoundingClientRect().bottom),
      cmdTop: cmd ? Math.round(cmd.getBoundingClientRect().top) : null,
      rows,
    };
  });
  await page.evaluate(() => window.__nxvim.execLua("nx.cmdline_complete.setup {}"));
  await page.evaluate(() => { window.__nxvim.feed("<Esc>"); document.getElementById("kbd").focus(); });
  await page.keyboard.type(":tab");
  await page.keyboard.press("Tab"); // open the wildmenu + highlight the top fuzzy match
  // The command catalog resolves through an async Lua source; poll for the full list.
  let wild = null;
  for (let i = 0; i < 40; i++) {
    wild = await wildSnap();
    if (wild && wild.rows.length >= 3 && wild.rows.some((r) => r.text === "tabnew")) break;
    await sleep(50);
  }
  const { ch } = await page.evaluate(() => window.__nxvim.cellMetrics());

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

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — DOM renderer + mouse + selection/cursor + layout + highlighting + wildmenu verified in a real browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
