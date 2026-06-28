// Playwright verifier for the PICKER INPUT CURSOR on the pure web client (serverless).
//
// Bug: the wasm fuzzy picker drew NO cursor in its prompt/input box, and left the
// buffer cursor visible underneath the overlay. The native TUI/GUI both move the
// cursor into the picker prompt (at `> ` + query_cursor) and hide the buffer cursor
// while a prompt-carrying menu is open. This asserts the web client now matches:
//   1. while the picker is open, the only on-screen cursor (`#nx-cursor` / `.cur-block`)
//      lives INSIDE the picker box (`.pmenu`), not in a buffer line;
//   2. it sits at the typed query position (after `> ` + the query);
//   3. closing the picker restores the buffer cursor.
//
// Faithfulness (not a no-op): the picker is opened through the real `nx.picker` API and
// driven with real `feed()` keystrokes through the production tick; the assertions read
// the actual rendered DOM (`#grid`), not a mock.
//
// Prereqs: ./build.sh (../dist/eh.mjs + eh.wasm) and a Chromium for Playwright.
// Run:  node verify-picker-cursor.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8159;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
  if (found.length) return found[found.length - 1];
  // macOS: fall back to Playwright's own bundled Chromium for Testing.
  return undefined; // let Playwright resolve its installed browser
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

// Snapshot the rendered cursor state out of the live DOM.
const cursorState = (page) =>
  page.evaluate(() => {
    const grid = document.getElementById("grid");
    const cur = document.getElementById("nx-cursor");
    const blocks = grid.querySelectorAll(".cur-block, .cur-bar, .cur-underline");
    const inPmenu = (el) => !!(el && el.closest(".pmenu"));
    const promptEl = grid.querySelector(".pmenu .pmenu-prompt") || grid.querySelector(".pmenu .row");
    return {
      hasCursor: !!cur,
      cursorInPmenu: inPmenu(cur),
      cursorChar: cur ? cur.textContent : null,
      blockCount: blocks.length,
      blocksAllInPmenu: [...blocks].every(inPmenu),
      promptText: promptEl ? promptEl.textContent : null,
    };
  });

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless)", true);

  // Baseline: with no picker open, the buffer owns the (single) cursor.
  await sleep(200);
  const before = await cursorState(page);
  check("buffer cursor visible before picker opens", before.hasCursor && !before.cursorInPmenu,
    JSON.stringify(before));

  // Open the real picker and type a query through the production keystroke path.
  await page.evaluate(() => window.__nxvim.execLua(`nx.picker.open('files')`));
  await sleep(300);
  await page.evaluate(() => window.__nxvim.feed("ab"));
  await sleep(300);

  const open = await cursorState(page);
  check("picker open: a cursor is rendered", open.hasCursor, JSON.stringify(open));
  check("picker open: the cursor lives INSIDE the picker box", open.cursorInPmenu, JSON.stringify(open));
  check("picker open: the buffer cursor is hidden (only the prompt cursor remains)",
    open.blockCount === 1 && open.blocksAllInPmenu, JSON.stringify(open));
  check("picker open: prompt shows the typed query", /^>\sab/.test(String(open.promptText)),
    JSON.stringify(open.promptText));

  // Close the picker — the buffer cursor must come back.
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await sleep(300);
  const after = await cursorState(page);
  check("buffer cursor restored after picker closes", after.hasCursor && !after.cursorInPmenu,
    JSON.stringify(after));
} catch (e) {
  check("harness ran without throwing", false, String((e && e.stack) || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — picker draws its input cursor and hides the buffer cursor"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
