// Playwright verifier for cursor rendering across a window split in the web edit-host.
// The native TUI/GUI clients draw a cursor ONLY in the focused window — an unfocused
// window shows no cursor at all (faithful to vim's single terminal cursor). The web
// renderer must match: after a split, exactly one window (the focused one) paints a
// cursor, and the unfocused window paints none. Regresses the "web draws inactive
// (hollow) cursors for unfocused windows" bug.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-inactive-cursor.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8123;
const ORIGIN = `http://localhost:${PORT}`;

// Prefer an explicit Chromium (PW_CHROMIUM), else the newest ms-playwright build for this
// platform, else Playwright's bundled default.
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
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// All cursor glyph classes the renderer can emit for a real (filled) cursor, plus the
// hollow outline the bug painted in unfocused windows.
const FILLED = ".cur-block, .cur-bar, .cur-underline";
const HOLLOW = ".cur-hollow";

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // Put some text in so each window has a cursor cell to land on, then split vertically.
  // After `<C-w>v` there are two windows; focus stays in one, the other is inactive.
  await page.evaluate(() => window.__bemtvi.feed("ihello world<Esc>gg0"));
  await page.evaluate(() => window.__bemtvi.feed("<C-w>v"));
  await sleep(300);

  const winCount = await page.evaluate(() => (window.__bemtvi.frame().windows || []).length);
  check("split produced two windows", winCount === 2, `windows=${winCount}`);

  const filled = await page.evaluate((sel) => document.querySelectorAll(sel).length, FILLED);
  const hollow = await page.evaluate((sel) => document.querySelectorAll(sel).length, HOLLOW);

  // Exactly one filled cursor — the focused window. The unfocused window draws none,
  // matching the native TUI/GUI clients.
  check("exactly one filled cursor (focused window only)", filled === 1, `filled=${filled}`);
  check("no cursor drawn in the unfocused window", hollow === 0, `hollow=${hollow}`);

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
