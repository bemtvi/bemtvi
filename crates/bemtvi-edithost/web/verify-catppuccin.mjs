// End-to-end check of the user-reported case: the **real** catppuccin colorscheme
// (mocha) applied in the web build now actually recolors the editor (docs/plans/
// 2026-06-24-web-colorscheme-bridge.md). Loads the vendored first-party plugin bundle
// (build-plugins.sh → web/vendor/plugins/plugins-bundle.lua) the same way worker.mjs
// does on the demo build, runs `require("catppuccin").load("mocha")`, and asserts the
// rendered chrome + syntax follow the mocha palette — not the built-in One Dark.
//
// Prereq: ./build.sh and ./build-plugins.sh (vendors the bundle incl. catppuccin).
//   node verify-catppuccin.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, existsSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8102;

if (!existsSync(`${here}vendor/plugins/plugins-bundle.lua`)) {
  console.log("SKIP — plugin bundle not vendored; run ./build-plugins.sh first");
  process.exit(0);
}

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

async function tokenColor(page, text) {
  for (let i = 0; i < 60; i++) {
    const c = await page.evaluate((t) => {
      const row = document.querySelector("#grid .win .row");
      if (!row) return null;
      const run = [...row.querySelectorAll("span")].find((s) => s.textContent === t);
      return run ? (run.getAttribute("style") || "").match(/color\s*:\s*([^;]+)/)?.[1]?.trim() ?? null : null;
    }, text);
    if (c) return c;
    await sleep(100);
  }
  return null;
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

  // First load: get an OPFS scope, then seed the real plugin bundle + an init.lua that
  // loads catppuccin mocha. The Worker sources `/plugins-bundle.lua` (registers each
  // plugin's package.preload[...]) then `/init.lua` at boot — exactly the demo's path,
  // minus Pyodide. (execLua-ing the 352K bundle over the RPC would block, so seed + boot
  // through the synchronous in-Worker source path the editor actually uses.)
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  const bundle = await (await fetch(`http://localhost:${PORT}/web/vendor/plugins/plugins-bundle.lua`)).text();
  await page.evaluate(async ({ bundle }) => {
    const root = await navigator.storage.getDirectory();
    const write = async (name, text) => {
      const fh = await root.getFileHandle(name, { create: true });
      const w = await fh.createWritable(); await w.write(text); await w.close();
    };
    await write("plugins-bundle.lua", bundle);
    await write("init.lua",
      'require("catppuccin").setup({ flavour = "mocha" })\n' +
      'require("catppuccin").load("mocha")\n');
  }, { bundle });

  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  const normalBg = await page.evaluate(() => window.__bemtvi
    .execLua('return tostring((vim.api.nvim_get_hl(0, { name = "Normal" }) or {}).bg)')
    .then((r) => JSON.stringify(r.result)));
  // #1e1e2e as the integer nvim_get_hl reports = 0x1e1e2e = 1973806.
  check("catppuccin loaded (Normal.bg = mocha base on the Lua side)", /1973806/.test(String(normalBg)), String(normalBg));

  await page.evaluate(() => window.__bemtvi.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__bemtvi.feed("ggdGifn main() {}<Esc>"));

  // Chrome: editor background = mocha base #1e1e2e (rgb 30,30,46).
  let bg = "";
  for (let i = 0; i < 60; i++) {
    bg = await page.evaluate(() => getComputedStyle(document.getElementById("grid")).backgroundColor);
    if (bg === "rgb(30, 30, 46)") break;
    await sleep(100);
  }
  check("chrome: editor bg is catppuccin mocha base #1e1e2e", bg === "rgb(30, 30, 46)", bg);

  // Syntax: `fn` follows @keyword → Keyword → mauve #cba6f7 (NOT One Dark #c678dd).
  const kw = await tokenColor(page, "fn");
  check("syntax: `fn` is catppuccin mauve #cba6f7 (not One Dark)", kw === "#cba6f7", kw);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — real catppuccin mocha applies in the web build"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
