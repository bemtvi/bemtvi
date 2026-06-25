// Playwright verifier for `:eo` (the native file-open picker) on the pure web client
// (serverless, no daemon). Typing `:eo<CR>` over the command line must pop the browser's
// File System Access picker, then `:e <name>` the chosen file so its bytes load into the
// buffer. The picker is stubbed (Playwright can't drive the OS chooser) to return a handle
// whose getFile() yields known content; the test asserts the content lands in the buffer.
//
// Faithfulness (not a no-op): drives REAL keydown events on the kbd element so the in-page
// `tryFilePickerIntercept` runs exactly as a user's keystrokes would; the only stub is the
// OS chooser itself. The buffer content is read back from the live editor view.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-eo.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8149;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`),
  ].sort();
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

const PICKED_NAME = "picked-eo.txt";
const PICKED_BODY = "PICKED-FILE-CONTENT\nsecond line\n";

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

  // Stub the OS file chooser BEFORE the page script computes `fsApiAvailable` and captures it.
  // The save handle records the bytes written to it (under `self.__saved`) so the `:wo` path
  // can be asserted; both pickers return the same handle shape.
  await page.addInitScript(({ name, body }) => {
    self.__saved = null;
    const handle = {
      name,
      kind: "file",
      async getFile() { return new File([body], name, { type: "text/plain" }); },
      async requestPermission() { return "granted"; },
      async queryPermission() { return "granted"; },
      async createWritable() {
        return {
          async write(data) {
            const buf = data instanceof Blob ? await data.arrayBuffer() : data;
            self.__saved = new TextDecoder().decode(new Uint8Array(buf));
          },
          async close() {}, async truncate() {},
        };
      },
    };
    self.showOpenFilePicker = async () => [handle];
    self.showSaveFilePicker = async () => handle;
  }, { name: PICKED_NAME, body: PICKED_BODY });

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless)", true);

  // Focus the kbd element so the real keydown handler (and its `:eo` intercept) fires.
  await page.evaluate(() => document.getElementById("kbd").focus());

  // Type `:eo` and wait until the command line shows it before pressing Enter (the intercept
  // reads the latest frame's cmdline at Enter time).
  await page.keyboard.type(":eo");
  const cmd = await (async () => {
    for (let i = 0; i < 100; i++) {
      const v = await page.evaluate(() => window.__nxvim.cmdline());
      if (v === ":eo") return v;
      await sleep(30);
    }
    return await page.evaluate(() => window.__nxvim.cmdline());
  })();
  check("command line shows `:eo` before Enter", cmd === ":eo", `cmdline=${JSON.stringify(cmd)}`);

  await page.keyboard.press("Enter");

  // The picker resolves, binds the path, and runs `:e picked-eo.txt`; the worker round-trips
  // the bound read back to the page. Poll the buffer for the picked content.
  const lines = await (async () => {
    for (let i = 0; i < 150; i++) {
      const v = await page.evaluate(() => window.__nxvim.lines());
      if (/PICKED-FILE-CONTENT/.test(String(v))) return v;
      await sleep(40);
    }
    return await page.evaluate(() => window.__nxvim.lines());
  })();
  check(":eo opens the picked file into the buffer",
    /PICKED-FILE-CONTENT/.test(String(lines)) && /second line/.test(String(lines)),
    `lines=${JSON.stringify(lines)}`);

  // Also confirm the command line cleared (the intercept consumed the <CR>, didn't run `:eo`
  // verbatim — which would error as an unknown command).
  const finalMsg = await page.evaluate(() => window.__nxvim.message());
  check("no error message (`:eo` was intercepted, not run as an unknown ex-command)",
    !/E\d|not.*editor command|unknown/i.test(String(finalMsg)), `message=${JSON.stringify(finalMsg)}`);

  // ── `:wo` save picker — edit the buffer, then `:wo` writes the buffer through the bound
  // handle (the same cwd-resolved key path the open picker uses). ──────────────────────────
  await page.evaluate(() => document.getElementById("kbd").focus());
  await page.keyboard.press("Escape");
  await page.keyboard.type("ggIEDIT-");           // insert a marker at the buffer's start
  await page.keyboard.press("Escape");
  await page.keyboard.type(":wo");
  for (let i = 0; i < 100; i++) {
    if ((await page.evaluate(() => window.__nxvim.cmdline())) === ":wo") break;
    await sleep(30);
  }
  await page.keyboard.press("Enter");
  const saved = await (async () => {
    for (let i = 0; i < 150; i++) {
      const v = await page.evaluate(() => self.__saved);
      if (v && /EDIT-PICKED-FILE-CONTENT/.test(String(v))) return v;
      await sleep(40);
    }
    return await page.evaluate(() => self.__saved);
  })();
  check(":wo writes the edited buffer through the bound handle",
    saved != null && /EDIT-PICKED-FILE-CONTENT/.test(String(saved)), `saved=${JSON.stringify(saved)}`);

  await browser.close();
} catch (e) {
  check("harness ran without throwing", false, String((e && e.stack) || e));
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — :eo pops the picker and loads the chosen file on the serverless web client"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
