// Playwright verifier for slice 5c (the first half of the Phase 5 exit criteria):
// drive the **real** wasm edit-host running in a Web Worker, through the
// `window.__nxvim` hook, in a real (headless Chromium) browser — type vim commands and
// assert the buffer, the cursor, and the rendered `redraw` frame. Proves the
// Worker + postMessage redraw transport end-to-end, not in node but in a browser.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify.mjs            # boots serve.mjs on an ephemeral port, then drives it
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8099;

// Prefer an explicitly-installed Chromium (PW_CHROMIUM, else the newest
// ms-playwright `chromium-*/chrome` build) so the run doesn't depend on this exact
// Playwright version's bundled-browser revision. Falls back to Playwright's default.
function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

// 1. Start the dev server (COOP/COEP) on a fixed port.
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  // Wait for the listener.
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);

  // Cross-origin isolation must hold (SAB prerequisite for 5d; proven here).
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (COOP/COEP)", isolated === true, `crossOriginIsolated=${isolated}`);

  // The worker boots, the host inits, the first frame renders.
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // 2. Insert text via vim keys through the worker; the real tick runs in the browser.
  await page.evaluate(() => window.__nxvim.feed("ihello world<Esc>"));
  const lines = await page.evaluate(() => window.__nxvim.lines());
  check("editor: insert via vim keys → buffer line", lines === "hello world", `got ${JSON.stringify(lines)}`);

  // 3. The cursor settled where vim leaves it after <Esc> (on the last typed char, col 10).
  const cursor = await page.evaluate(() => window.__nxvim.cursor());
  check("cursor: <Esc> after insert sits on last char", cursor && cursor.col === 10, JSON.stringify(cursor));

  // 4. The real redraw frame, projected by the server view through the wasm tick and
  //    ferried over postMessage, shows the text in its grid (read off the DOM the UI
  //    rendered — proving the transport + renderer, not just the FFI return value).
  const gridText = await page.evaluate(() => document.getElementById("grid").textContent);
  check("redraw: rendered grid shows the buffer text", gridText.includes("hello world"), gridText.split("\n")[0]);

  // 5. A motion + operator (delete a word) drives more ticks; buffer + frame update.
  await page.evaluate(() => window.__nxvim.feed("0dw"));
  const afterDw = await page.evaluate(() => window.__nxvim.lines());
  check("editor: 0dw deletes the first word", afterDw === "world", `got ${JSON.stringify(afterDw)}`);

  // 6. Lua drives the editor through the real effects path, in the browser.
  await page.evaluate(() => window.__nxvim.execLua('vim.cmd("%s/world/wasm/")'));
  const afterSub = await page.evaluate(() => window.__nxvim.lines());
  check("lua → editor: :substitute via real effects", afterSub === "wasm", `got ${JSON.stringify(afterSub)}`);

  // 7. Command-line mode renders on the bottom row with the ':' prompt.
  await page.evaluate(() => window.__nxvim.feed(":set number"));
  const cmdline = await page.evaluate(() => window.__nxvim.cmdline());
  check("redraw: command-line mode shows ':' prompt + text", cmdline === ":set number", `got ${JSON.stringify(cmdline)}`);
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  // 8. Slice 5d — the SAB input/timer loop. A deferred timer fires on its own (no
  //    further input) when the Worker's Atomics.wait park times out at the timer's
  //    deadline — the second half of the Phase 5 exit criteria.
  const sabActive = await page.evaluate(() => window.__nxvim.sab === true);
  check("slice 5d: SAB input/timer loop is active (cross-origin isolated)", sabActive, `sab=${sabActive}`);

  if (sabActive) {
    // Buffer currently reads "wasm" (from the :substitute above). Arm a one-shot
    // vim.defer_fn that rewrites it 150ms from now, via the proven vim.cmd path.
    await page.evaluate(() =>
      window.__nxvim.execLua('vim.defer_fn(function() vim.cmd("%s/wasm/timerfired/") end, 150)'),
    );
    const before = await page.evaluate(() => window.__nxvim.lines());
    check("timer: not fired immediately after scheduling", before === "wasm", `got ${JSON.stringify(before)}`);

    // Wait WITHOUT sending any further input. The only thing that can mutate the buffer
    // now is the Worker firing the deferred callback off its park timeout.
    await sleep(600);
    const after = await page.evaluate(() => window.__nxvim.lines());
    check("timer: deferred callback fired on its own via the SAB park", after === "timerfired", `got ${JSON.stringify(after)}`);

    // A self-rescheduling defer_fn chain fires repeatedly with no input — each one-shot
    // re-arms the next, so the wheel must wake and fire several times unattended.
    await page.evaluate(() =>
      window.__nxvim.execLua(
        "_G.__ticks = 0; " +
          "local function tick() _G.__ticks = _G.__ticks + 1; " +
          "if _G.__ticks < 6 then vim.defer_fn(tick, 50) end end; " +
          "vim.defer_fn(tick, 50)",
      ),
    );
    await sleep(600);
    const ticks = await page.evaluate(() => window.__nxvim.execLua("return _G.__ticks").then((r) => r.result));
    const n = parseInt(String(ticks).replace(/^ok:/, ""), 10);
    check("timer: a self-rescheduling defer_fn chain fired repeatedly unattended", n >= 5, `ticks=${JSON.stringify(ticks)} (parsed ${n})`);
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — wasm edit-host driven in a real browser via window.__nxvim (slice 5c)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
