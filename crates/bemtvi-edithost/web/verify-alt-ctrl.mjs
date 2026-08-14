// Playwright verifier for the Alt-stands-in-for-Ctrl remap in the web edit-host.
//
// Chrome/Edge on Windows and Linux handle a handful of Ctrl chords ahead of the page
// (`<C-w>` close tab, `<C-t>`/`<C-n>` new tab/window, `<C-Tab>` and `<C-1>`..`<C-9>` tab
// switching), so the page's `preventDefault()` never gets a say and the editor never sees
// the key. macOS hangs those on Cmd instead, leaving Ctrl free — which is why `<C-w>`
// worked there and only there. The client therefore accepts Alt as the stand-in for
// exactly those chords on non-Mac platforms, and remaps nothing on a Mac.
//
// This drives REAL keydown events (page.keyboard, through the focused #kbd proxy) rather
// than the `window.__bemtvi.feed` hook, because the whole point under test is the browser
// event → notation encoding, which `feed` bypasses. The Mac branch is exercised by
// spoofing `navigator.platform`/`userAgentData` before the page's script runs.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-alt-ctrl.mjs
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

const windowCount = (page) =>
  page.evaluate(() => (window.__bemtvi.frame()?.windows || []).length);

// The first frame lands a tick after `ready` resolves, so a bare `windowCount` right
// after boot can legitimately read 0. Poll for the expected layout and return whatever
// count we ended on — deliberately no throw, so a run where the remap is broken still
// reports every check instead of aborting the harness at the first setup step.
async function waitForWindows(page, n) {
  let got = -1;
  for (let i = 0; i < 100; i++) {
    got = await windowCount(page);
    if (got === n) return got;
    await sleep(50);
  }
  return got;
}

// A fresh page on the given spoofed platform, booted and focused on the key proxy.
async function open(browser, { mac }) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  // Runs before the page's own script, so IS_MAC is computed from these.
  await page.addInitScript((isMac) => {
    const platform = isMac ? "MacIntel" : "Linux x86_64";
    Object.defineProperty(navigator, "platform", { get: () => platform });
    Object.defineProperty(navigator, "userAgentData", {
      get: () => ({ platform: isMac ? "macOS" : "Linux" }),
    });
  }, mac);
  await page.goto(`${ORIGIN}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await page.evaluate(() => document.getElementById("kbd").focus());
  await waitForWindows(page, 1);
  return page;
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });

  // ── Non-Mac: Super stands in for the stolen Ctrl chords ────────────────────────
  {
    const page = await open(browser, { mac: false });

    // Alt+w then `v` — `<C-w>v` is the vertical split. A real Ctrl+w would work here
    // too (headless has no tab to close), but on a real Chrome it never arrives, which
    // is exactly the hole this fills.
    await page.keyboard.press("Alt+w");
    await page.keyboard.press("v");
    await sleep(300);
    check("Alt+w v splits vertically (fed as `<C-w>v`)",
      (await windowCount(page)) === 2, `windows=${await windowCount(page)}`);

    // …and Alt+w c closes it again, so the chord is the real prefix, not a one-off.
    // The split is re-seeded through the `feed` hook rather than inherited from the check
    // above, so this assertion still bites when that one has already failed (closing a
    // window that was never split would otherwise "pass" by doing nothing).
    // `<Esc>` first: if the check above failed, its stray `v` left the editor in Visual
    // and the seed below would land somewhere else entirely.
    await page.evaluate(() => window.__bemtvi.feed("<Esc><C-w>o"));  // back to one, either way
    await waitForWindows(page, 1);
    await page.evaluate(() => window.__bemtvi.feed("<C-w>v"));
    const seeded = await waitForWindows(page, 2);
    check("seed: the split to close is really there", seeded === 2, `windows=${seeded}`);
    await page.keyboard.press("Alt+w");
    await page.keyboard.press("c");
    await sleep(300);
    check("Alt+w c closes the split (fed as `<C-w>c`)",
      (await windowCount(page)) === 1, `windows=${await windowCount(page)}`);

    // An Alt chord OUTSIDE the reserved set still encodes as Alt. `<A-c>` is the
    // multi-cursor placement mode, so this asserts a real effect rather than the absence
    // of one — a widened substitution would send `<C-c>` and never reach MULTICURSOR.
    await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
    await sleep(200);
    await page.keyboard.press("Alt+c");
    await sleep(300);
    const altMode = await page.evaluate(() => window.__bemtvi.mode());
    check("Alt+c (not a reserved chord) still encodes as `<A-c>` — enters MULTICURSOR",
      altMode === "MULTICURSOR", `mode=${JSON.stringify(altMode)}`);
    await page.keyboard.press("Escape");
    await sleep(200);

    // Plain Ctrl still encodes as Ctrl on non-Mac — the remap ADDS a path, it doesn't
    // replace one. (Headless has no tab for Ctrl+w to close, so the real chord does reach
    // the page here; on a real Chrome it wouldn't, which is the whole point of the Super
    // stand-in above.)
    await page.keyboard.press("Control+w");
    await page.keyboard.press("v");
    await sleep(300);
    check("plain Ctrl still encodes as Ctrl on non-Mac (`<C-w>v` splits)",
      (await windowCount(page)) === 2, `windows=${await windowCount(page)}`);
    await page.close();
  }

  // ── Mac: nothing is remapped; Ctrl arrives on its own and Cmd stays the browser's ──
  {
    const page = await open(browser, { mac: true });

    // Alt is Option on a Mac (it composes characters); it must NOT become a Ctrl
    // stand-in there. Were it remapped, the following `v` would split.
    await page.keyboard.press("Alt+w");
    await page.keyboard.press("v");
    await sleep(300);
    check("Alt+w v does NOT split on mac (Alt stays Option)",
      (await windowCount(page)) === 1, `windows=${await windowCount(page)}`);

    // Ctrl+w is free on macOS, so the real chord splits with no remap involved.
    await page.keyboard.press("Escape");
    await page.keyboard.press("Control+w");
    await page.keyboard.press("v");
    await sleep(300);
    check("Ctrl+w v splits on mac (the chord the browser leaves alone)",
      (await windowCount(page)) === 2, `windows=${await windowCount(page)}`);
    await page.close();
  }

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
