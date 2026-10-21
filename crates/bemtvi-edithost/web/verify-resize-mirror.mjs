// Playwright verifier: a UI re-attach at a new size reaches Lua at once on the web build,
// as a resize does natively. The page attaches at a default size in `eh_new`, then again
// at its real size; the second attach used to repaint at the new size without resizing
// the core's windows, so `WinResized` never fired and Lua's window mirror kept the old
// size until the next keypress. A plugin laying itself out on the screen (the intro
// screen centring its logo) drew for the wrong size.
//
//   node verify-resize-mirror.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8143;

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

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await sleep(300);

  const lua = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => String(r.result)), code);
  const frameWidth = () => page.evaluate(() => window.__bemtvi.frame().windows[0].rect.width);

  // ---- 1. After boot, Lua sees the window the frame paints ----
  const fw = await frameWidth();
  const lw = await lua("return btv.win.width(0)");
  check("boot: btv.win.width matches the painted window", lw.includes(`(${fw})`) || lw.endsWith(`:${fw}`) || lw === String(fw), `lua=${lw} frame=${fw}`);

  // ---- 2. A re-attach at a new size fires WinResized and updates the mirror at once ----
  await lua("_G.resized = 0 btv.on('WinResized', function() _G.resized = _G.resized + 1 end) return 1");
  await page.evaluate(() => window.__bemtvi.attach(60, 20));
  await sleep(300);
  const after = await lua("return btv.win.width(0) .. 'x' .. btv.win.height(0) .. ' resized ' .. _G.resized");
  check("re-attach: btv.win.width/height follow it without a keypress", /60x\d+/.test(after), after);
  check("re-attach: WinResized fired", /resized [1-9]/.test(after), after);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS — a re-attach reaches Lua at once" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
