// Playwright verifier for IME caret tracking in the web edit-host.
//
// The OS positions the IME candidate window and the macOS press-and-hold accent menu at the
// *focused element's* caret. The grid is a non-editable <div>, so keyboard focus lives on an
// off-screen proxy — and the editor draws its own cursor, so the proxy's caret must be made
// to coincide with the drawn cursor. Two paths:
//   • Chrome (EditContext): Chrome won't re-report a CSS-moved textarea's caret bounds, so
//     the IME popup pins to the textarea's focus-time origin (the top-left bug). The fix
//     reports the cursor bounds explicitly via `EditContext.updateSelectionBounds`, and
//     receives composed / accented text via `textupdate` (keydown is 229/`Process`).
//   • Firefox (no EditContext): park the hidden textarea over the cursor; the OS reads its
//     caret there live.
//
// The OS-drawn popup isn't observable from the page, so this asserts on (a) the bounds we
// hand the browser tracking the cursor, and (b) composed/accented text — driven through the
// real Chrome IME via CDP — actually reaching the editor buffer.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-ime-position.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8131;
const ORIGIN = `http://localhost:${PORT}`;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const pats = [
    `${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`,
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
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// The cursor cell's on-screen rect and the bounds we last reported to the IME.
async function state(page) {
  return page.evaluate(() => {
    const cur = document.getElementById("nx-cursor");
    const cr = cur ? cur.getBoundingClientRect() : null;
    return {
      cur: cr && { left: cr.left, top: cr.top },
      ime: window.__nxvim.ime(),
    };
  });
}
const near = (a, b, tol = 2) =>
  a && b && Math.abs(a.left - b.left) <= tol && Math.abs(a.top - b.top) <= tol;

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Chromium supports EditContext, so the Chrome path must be the one in use.
  const usingEditContext = await page.evaluate(() => window.__nxvim.ime().editContext);
  check("EditContext path active on Chromium", usingEditContext === true);

  // Type a few lines so the cursor can sit well away from the top-left origin.
  await page.evaluate(() => window.__nxvim.feed("ifirst line<CR>second line<CR>third line here<Esc>gg0"));
  await sleep(200);

  let s = await state(page);
  check("cursor cell tagged with #nx-cursor", s.cur !== null, JSON.stringify(s));
  check("IME caret reported at the cursor (top-left)", near(s.cur, s.ime.rect), JSON.stringify(s));

  // Move the cursor down/right — the reported IME bounds must follow, not stay at the origin.
  await page.evaluate(() => window.__nxvim.feed("jj0fh")); // line 3, onto the 'h' of "here"
  await sleep(200);
  s = await state(page);
  check("IME caret follows the cursor after moving", near(s.cur, s.ime.rect), JSON.stringify(s));
  check("IME caret moved off the top-left origin",
    s.ime.rect && (s.ime.rect.top > 5 || s.ime.rect.left > 5), JSON.stringify(s.ime.rect));

  // Insert mode mid-line — still tracked.
  await page.evaluate(() => window.__nxvim.feed("<Esc>2G0wi"));
  await sleep(200);
  s = await state(page);
  check("IME caret tracks the insert-mode cursor", near(s.cur, s.ime.rect), JSON.stringify(s));

  // ---- Real IME input through Chrome, driven via CDP ----
  // Drive the actual EditContext input path (not the feed() hook) to prove composed and
  // accented text reach the editor buffer. Start from a clean buffer in insert mode.
  await page.evaluate(() => window.__nxvim.feed("<Esc>ggVGd"));   // clear buffer
  await page.evaluate(() => window.__nxvim.feed("i"));            // insert mode
  await page.evaluate(() => document.getElementById("ime").focus());
  await sleep(100);
  const cdp = await page.context().newCDPSession(page);

  // Accent-menu style: a direct commit with no surrounding composition (textupdate, no
  // compositionstart) — the case the user reported (press-and-hold accents).
  await cdp.send("Input.insertText", { text: "é" });
  await sleep(200);
  let lines = await page.evaluate(() => window.__nxvim.lines());
  check("direct IME insert (accent menu) reaches the buffer", String(lines).includes("é"), JSON.stringify(lines));

  // Composition style (CJK): an active composition, then a commit.
  await cdp.send("Input.imeSetComposition", { text: "ぺ", selectionStart: 1, selectionEnd: 1 });
  await sleep(100);
  await cdp.send("Input.insertText", { text: "日本" });
  await sleep(200);
  lines = await page.evaluate(() => window.__nxvim.lines());
  check("composed IME text (commit) reaches the buffer", String(lines).includes("日本"), JSON.stringify(lines));

  // Normal keydown typing must still work on the EditContext host (the keydown→feed path).
  await page.evaluate(() => window.__nxvim.feed("<Esc>ggVGd i"));
  await page.evaluate(() => document.getElementById("ime").focus());
  await page.keyboard.type("abc");
  await sleep(200);
  lines = await page.evaluate(() => window.__nxvim.lines());
  check("normal keydown typing still inserts", String(lines).includes("abc"), JSON.stringify(lines));

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
