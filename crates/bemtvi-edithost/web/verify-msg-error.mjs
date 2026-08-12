// Playwright verifier for the red error message line (the `message_error` redraw
// flag). Drives the real wasm edit-host in headless Chromium and asserts that an
// error message (a command error / `:echoerr` / `btv.err_write`) paints the cmdline
// row red, while a plain `:echo` does not. Companion to verify-ui.mjs.
//
//   node verify-msg-error.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8099;

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

// Feed keys, then wait until the message hook reports the expected text.
async function feedAwaitMessage(page, keys, needle) {
  await page.evaluate((k) => window.__bemtvi.feed(k), keys);
  for (let i = 0; i < 60; i++) {
    const msg = await page.evaluate(() => window.__bemtvi.message());
    if (msg.includes(needle)) return msg;
    await sleep(50);
  }
  return await page.evaluate(() => window.__bemtvi.message());
}

// The inline color of the `.cmdline` row (the error paint sets `el.style.color`).
function cmdlineColor(page) {
  return page.evaluate(() => {
    const el = document.querySelector("#grid .cmdline");
    return el ? el.style.color : null;
  });
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

  // ---- 1. A command error flags the message red ----
  await feedAwaitMessage(page, ":nosuchcommand<CR>", "E492");
  let err = await page.evaluate(() => window.__bemtvi.messageError());
  let color = await cmdlineColor(page);
  check("command error: messageError() is true", err === true, `err=${err}`);
  check("command error: cmdline row painted red", !!color, `color=${color}`);

  // ---- 2. btv.err_write flags the message red ----
  await feedAwaitMessage(page, ":lua btv.err_write('lua boom')<CR>", "lua boom");
  err = await page.evaluate(() => window.__bemtvi.messageError());
  color = await cmdlineColor(page);
  check("btv.err_write: messageError() is true", err === true, `err=${err}`);
  check("btv.err_write: cmdline row painted red", !!color, `color=${color}`);

  // ---- 3. A plain :echo is NOT flagged / not red ----
  await feedAwaitMessage(page, ":echo 'all good'<CR>", "all good");
  err = await page.evaluate(() => window.__bemtvi.messageError());
  color = await cmdlineColor(page);
  check("plain :echo: messageError() is false", err === false, `err=${err}`);
  check("plain :echo: cmdline row not painted red", !color, `color=${color}`);

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e?.stack || e);
  failures++;
}

cleanup();
process.exit(failures ? 1 : 0);
