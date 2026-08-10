// Playwright verifier for single-file init.lua sourcing from OPFS. Writes an /init.lua
// into the browser's Origin Private File System, reloads the page, and asserts the
// config took effect on startup: an option set, a keymap that fires, and a VimEnter
// autocmd that ran (proving config sources BEFORE VimEnter, the native ordering). Also
// checks a broken config is surfaced (non-fatal) rather than bricking the editor.
//
//   node verify-config.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8092;

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

// Write `text` to OPFS path `/name` from inside the page (raw OPFS API — a path the
// editor never touches), so the next page load's Worker reads it as the config.
async function writeOpfs(page, name, text) {
  await page.evaluate(async ({ name, text }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(name, { create: true });
    const w = await fh.createWritable();
    await w.write(text);
    await w.close();
  }, { name, text });
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // First load just to get an OPFS handle scope, then write the config + reload so the
  // Worker sources it at boot.
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  const INIT_LUA = [
    "vim.o.tabstop = 7",                                  // an option
    "vim.g.__cfg = 'loaded'",                             // a global marker
    "vim.keymap.set('n', 'Q', 'ihi-from-config<Esc>')",  // a normal-mode keymap
    // A BufEnter autocmd: it fires for the STARTUP buffer (emitted by boot_finish), so
    // it only runs if the config sourced BEFORE boot finished — the ordering proof.
    "vim.api.nvim_create_autocmd('BufEnter', { callback = function() vim.g.__bufenter = 'fired' end })",
    // A UIEnter autocmd: fired when the UI attaches, with nx.ui.caps() already
    // refreshed. A browser delivers every chord distinctly, so keyboard_protocol is
    // true here — the fact a plugin gates a <C-h>-class mapping on.
    "nx.on('UIEnter', {}, function() vim.g.__uienter = tostring(nx.ui.caps().keyboard_protocol) end)",
  ].join("\n");
  await writeOpfs(page, "init.lua", INIT_LUA);

  // Reload: the Worker boots fresh and sources /init.lua before finishing startup.
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  const luaResult = (code) => page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

  // 1. The option set in init.lua is in effect.
  const ts = await luaResult("return vim.o.tabstop");
  check("config: vim.o.tabstop set by init.lua", /\b7\b/.test(String(ts)), `tabstop=${JSON.stringify(ts)}`);

  // 2. The config actually ran (its global marker is set).
  const marker = await luaResult("return vim.g.__cfg or 'nil'");
  check("config: init.lua ran (vim.g marker set)", /loaded/.test(String(marker)), `marker=${JSON.stringify(marker)}`);

  // 3. The BufEnter autocmd registered in the config fired for the startup buffer —
  //    proving the config sourced BEFORE boot finished (boot_finish emits the startup
  //    buffer's BufEnter), not after.
  const bufenter = await luaResult("return vim.g.__bufenter or 'nil'");
  check("config: a startup-buffer BufEnter autocmd in init.lua fired (config-before-boot-finish)",
    /fired/.test(String(bufenter)), `bufenter=${JSON.stringify(bufenter)}`);

  // 4. The UIEnter autocmd fired at attach, and read the browser client's caps. This is
  //    the web half of the tier-1 rule: a plugin that installs capability-dependent
  //    keymaps on UIEnter behaves here exactly as it does natively.
  const uienter = await luaResult("return vim.g.__uienter or 'nil'");
  check("config: a UIEnter autocmd fired with nx.ui.caps() populated",
    /true/.test(String(uienter)), `uienter=${JSON.stringify(uienter)}`);

  // 5. A keymap defined in the config works end to end: press Q → it inserts text.
  await page.evaluate(() => window.__nxvim.feed("ggdGQ"));
  const afterMap = await page.evaluate(() => window.__nxvim.lines());
  check("config: a keymap from init.lua fires on keypress",
    afterMap === "hi-from-config", `lines=${JSON.stringify(afterMap)}`);

  // 6. A broken config is surfaced, non-fatal: the editor still boots and edits.
  await writeOpfs(page, "init.lua", "this is not valid lua <<<");
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  await page.evaluate(() => window.__nxvim.feed("ggdGistill-works<Esc>"));
  const afterBroken = await page.evaluate(() => window.__nxvim.lines());
  check("config: a broken init.lua is non-fatal (editor still boots + edits)",
    afterBroken === "still-works", `lines=${JSON.stringify(afterBroken)}`);

  // Clean up the OPFS config so a re-run starts clean.
  await page.evaluate(async () => {
    try { (await navigator.storage.getDirectory()).removeEntry("init.lua"); } catch {}
  });

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — single-file init.lua sourced from OPFS (options, keymaps, startup BufEnter, UIEnter caps), broken config non-fatal"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
