// Playwright verifier for the serverless `"+` / `"*` clipboard registers in the browser
// edit-host. The synchronous `Clipboard` seam can't await `navigator.clipboard`, so the wasm
// build bridges through the Sink: a `"+`/`"*` yank/delete is forwarded to the UI thread and
// written to `navigator.clipboard` (eh_take_clipboard_writes → clipboard_write); the UI reads
// the OS clipboard back into the mirror a `"+p` consumes (eh_clipboard_push). This drives both
// directions through a real (headless Chromium) browser against the actual OS clipboard.
//
// Runs over the SAB transport (cross-origin isolated), so it also covers the ring type-8
// clipboard-push frame and the run loop's drainClipboardWrites convergence.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-clipboard.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8108;
const ORIGIN = `http://localhost:${PORT}`;

// Prefer an explicit Chromium (PW_CHROMIUM), else the newest ms-playwright build for this
// platform (linux `chrome-linux/chrome` or macOS `chrome-mac/Chromium.app/...`), else
// Playwright's bundled default.
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

// Poll `navigator.clipboard.readText()` until it equals `want` (the write is fire-and-forget,
// so it lands a tick after the feed resolves). Returns the last value seen.
async function clipboardEquals(page, want, tries = 50) {
  let last = "";
  for (let i = 0; i < tries; i++) {
    last = await page.evaluate(() => navigator.clipboard.readText());
    if (last === want) return last;
    await sleep(20);
  }
  return last;
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

async function waitReady(page) {
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const context = await browser.newContext();
  // The `"+` registers need both read (paste) and write (yank) permission for this origin.
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: ORIGIN });
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/index.html`);
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await waitReady(page);

  // ── Yank to `"+` writes the OS clipboard (linewise) ──────────────────────────────────────
  await page.evaluate(() => window.__nxvim.feed("ggdGiCOPY-FROM-NXVIM<Esc>"));
  await page.evaluate(() => window.__nxvim.feed('"+yy'));
  const wrote = await clipboardEquals(page, "COPY-FROM-NXVIM\n");
  check(
    'yank: `"+yy` wrote the line to navigator.clipboard (linewise → trailing \\n)',
    wrote === "COPY-FROM-NXVIM\n",
    `clipboard=${JSON.stringify(wrote)}`,
  );

  // ── Charwise yank to `"+` writes without a trailing newline ──────────────────────────────
  await page.evaluate(() => window.__nxvim.feed("ggdGiword charwise<Esc>"));
  await page.evaluate(() => window.__nxvim.feed('0"+yw')); // yank "word " (charwise, no \n)
  const wroteChar = await clipboardEquals(page, "word ");
  check(
    'yank: charwise `"+yw` wrote the word with no trailing newline',
    wroteChar === "word ",
    `clipboard=${JSON.stringify(wroteChar)}`,
  );

  // ── Paste from `"+` reads an external copy ───────────────────────────────────────────────
  // Put text on the OS clipboard "from another app", refresh the mirror (the focus/click
  // triggers do this in normal use), then `"+p` it into a fresh buffer.
  await page.evaluate(() => navigator.clipboard.writeText("PASTED-FROM-OUTSIDE"));
  await page.evaluate(() => window.__nxvim.clipboardRefresh());
  await page.evaluate(() => window.__nxvim.feed("ggdG"));
  await page.evaluate(() => window.__nxvim.feed('"+p'));
  const pasted = await page.evaluate(() => window.__nxvim.lines());
  check(
    'paste: `"+p` inserted the external clipboard text',
    pasted.includes("PASTED-FROM-OUTSIDE"),
    `lines=${JSON.stringify(pasted)}`,
  );

  // ── Linewise external copy pastes as a whole line ────────────────────────────────────────
  await page.evaluate(() => navigator.clipboard.writeText("EXTERNAL-LINE\n"));
  await page.evaluate(() => window.__nxvim.clipboardRefresh());
  await page.evaluate(() => window.__nxvim.feed("ggdGianchor<Esc>"));
  await page.evaluate(() => window.__nxvim.feed('"+p')); // linewise paste lands on a new line below
  const pastedLine = await page.evaluate(() => window.__nxvim.lines());
  check(
    'paste: linewise external copy `"+p` lands on its own line below the cursor',
    pastedLine === "anchor\nEXTERNAL-LINE",
    `lines=${JSON.stringify(pastedLine)}`,
  );

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless `\"+`/`\"*` clipboard works both directions (yank→navigator.clipboard, external copy→`\"+p`)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
