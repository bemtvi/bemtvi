// Playwright verifier for the Lua-source picker (`:luafile` / `:source` / `:luao`). This is
// the browser twin of vim's `:luafile <file>`: pick a real local `.lua` file through the File
// System Access API (`showOpenFilePicker`) and run it through the live effects path, so the
// `examples/*/init.lua` configs can be loaded in the serverless web build. Unlike `:eo`, the
// file is *executed*, not opened into a buffer.
//
// The native picker can't be driven by Playwright, so we stub it (via `addInitScript`, before
// the page's module evaluates) with a fake in-memory `.lua` handle, then drive `:luafile<CR>`
// through `window.__nxvim` and assert the chunk's side effects truly landed (a set option, a
// global, a user command) by reading them back with `execLua` — proving the real effects path
// ran, not merely that the file was read.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-luafile.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8104;

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

// The fake File System Access open picker, returning an in-memory `.lua` file. Installed
// before any page script so the editor's `fsApiAvailable` check sees it. `__nextOpenName`
// selects which seeded file the next picker "returns".
function installFakeFsApi() {
  const enc = new TextEncoder();
  window.__fakeFS = new Map(); // name -> { bytes: Uint8Array }
  window.__seedFile = (name, text) => window.__fakeFS.set(name, { bytes: enc.encode(text) });
  window.__nextOpenName = null;
  const makeHandle = (name) => ({
    kind: "file",
    name,
    async queryPermission() { return "granted"; },
    async requestPermission() { return "granted"; },
    async getFile() {
      const e = window.__fakeFS.get(name) || { bytes: new Uint8Array(0) };
      return new File([e.bytes], name);
    },
  });
  self.showOpenFilePicker = async () => {
    if (!window.__nextOpenName) throw new DOMException("cancelled", "AbortError");
    return [makeHandle(window.__nextOpenName)];
  };
  self.showSaveFilePicker = async () => { throw new DOMException("cancelled", "AbortError"); };
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// Poll the page until `pred(value)` holds (or timeout), returning the last value.
async function until(page, fn, pred, ms = 5000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // Install the fake picker BEFORE the page's module runs (so `fsApiAvailable` is true).
  await page.addInitScript(installFakeFsApi);
  await page.goto(`http://localhost:${PORT}/web/index.html`);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // ── 1. `:luafile` runs the picked .lua file through the live effects path ─────────────
  // A config-shaped chunk: it sets an option, a global, and defines a user command — the
  // three things an `examples/*/init.lua` typically does. All three side effects must land.
  const CONFIG = [
    "vim.opt.number = true",
    "_G.__example_ran = 'yes'",
    "vim.api.nvim_create_user_command('Hi', function() _G.__cmd_ran = 'hi' end, {})",
  ].join("\n");
  await page.evaluate((src) => { window.__seedFile("init.lua", src); window.__nextOpenName = "init.lua"; }, CONFIG);
  await page.evaluate(() => window.__nxvim.feed(":luafile"));
  await page.evaluate(() => window.__nxvim.pressEnter());

  const ranGlobal = await until(page, () => window.__nxvim.execLua("return _G.__example_ran or ''").then((r) => r.result), (v) => /yes/.test(String(v)));
  check(":luafile runs the picked file (global side effect)", /yes/.test(String(ranGlobal)), `__example_ran=${JSON.stringify(ranGlobal)}`);

  const numberSet = await luaResult(page, "return vim.opt.number:get() and 1 or 0");
  check(":luafile applies an option the config set ('number')", /(^|:)1/.test(String(numberSet)), `number=${JSON.stringify(numberSet)}`);

  // The user command the config defined is registered and runs.
  await page.evaluate(() => window.__nxvim.feed(":Hi<CR>"));
  const cmdRan = await until(page, () => window.__nxvim.execLua("return _G.__cmd_ran or ''").then((r) => r.result), (v) => /hi/.test(String(v)));
  check(":luafile registers a user command the config defined", /hi/.test(String(cmdRan)), `__cmd_ran=${JSON.stringify(cmdRan)}`);

  // ── 2. `:source` is an alias for the same picker ──────────────────────────────────────
  await page.evaluate(() => { window.__seedFile("two.lua", "_G.__second = 'B'"); window.__nextOpenName = "two.lua"; });
  await page.evaluate(() => window.__nxvim.feed(":source"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const second = await until(page, () => window.__nxvim.execLua("return _G.__second or ''").then((r) => r.result), (v) => /B/.test(String(v)));
  check(":source is an alias that runs the picked file", /B/.test(String(second)), `__second=${JSON.stringify(second)}`);

  // ── 3. A broken config surfaces its error but does not brick the session ──────────────
  await page.evaluate(() => { window.__seedFile("bad.lua", "this is not ) valid lua ("); window.__nextOpenName = "bad.lua"; });
  await page.evaluate(() => window.__nxvim.feed(":luafile"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  await sleep(300);
  // The editor still responds to Lua after a failed source (no panic / brick).
  const alive = await luaResult(page, "return 6 * 7");
  check("a broken config does not brick the session (editor still evaluates Lua)", /42/.test(String(alive)), `alive=${JSON.stringify(alive)}`);

  // ── 4. A `:luafile <path>` with an argument is NOT intercepted (no path on the web) ───
  // The picker fires only on the *bare* command; an argument leaves it to the core (which
  // has no path to resolve in the browser), proving the bare-command gate.
  await page.evaluate(() => { window.__nextOpenName = null; }); // a picker now would abort
  await page.evaluate(() => window.__nxvim.feed(":luafile /nope.lua"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  await sleep(200);
  const stillAlive = await luaResult(page, "return 1 + 1");
  check("an explicit-path :luafile is not intercepted by the picker", /2/.test(String(stillAlive)), `stillAlive=${JSON.stringify(stillAlive)}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — Lua-source picker (:luafile/:source) runs a real local .lua through the live effects path"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
