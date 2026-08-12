// Playwright verifier for the single-file plugin bundle (amalgamate-plugins.mjs + the boot
// path that sources /plugins-bundle.lua from OPFS before init.lua). The browser Lua VM has
// no filesystem/runtimepath, so a multi-file plugin can't load through package.path; the
// amalgamator concatenates each module into one chunk that registers package.preload, and
// `require` resolves from there. This proves the WHOLE mechanism end to end:
//
//   - a multi-file fixture plugin ("which-key": init + config + util, with init requiring
//     BOTH submodules) is amalgamated, seeded to OPFS, and `require("which-key").setup{}`
//     in init.lua runs at boot — its composed result (defaults merged + overridden + a
//     util transform) proves nested require, top-level locals as module scope, and the
//     module return value threading through `require`;
//   - the preload modules persist post-boot (a runtime `require` resolves too);
//   - a require of a missing module fails LOUD (no silent stub — CLAUDE.md);
//   - with the bundle ABSENT, `require("which-key")` fails — proving the modules truly come
//     from the bundle (no ambient leak) and that a missing bundle is non-fatal.
//
// Prereqs: ./build.sh (dist/eh.{mjs,wasm}) and a Chromium for Playwright. Run:
//   node verify-plugin-bundle.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { amalgamate } from "./amalgamate-plugins.mjs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8121;

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

// Build a realistic multi-file fixture plugin on disk, then amalgamate it. The shape mirrors
// a real first-party plugin: a root module that requires two submodules, defaults + a merge,
// and a string-formatting util — so the bundle exercises nested require + module scope.
function buildBundle() {
  const root = mkdtempSync(join(tmpdir(), "bemtvi-plug-"));
  const luaDir = join(root, "which-key", "lua", "which-key");
  mkdirSync(luaDir, { recursive: true });
  writeFileSync(join(luaDir, "init.lua"), [
    'local config = require("which-key.config")',
    'local util = require("which-key.util")',
    "local M = {}",
    "function M.setup(opts)",
    "  local merged = config.merge(opts)",
    "  _G.__wk_label = util.label(merged.name, merged.delay)",
    "end",
    "return M",
    "",
  ].join("\n"));
  writeFileSync(join(luaDir, "config.lua"), [
    "local M = {}",
    'M.defaults = { name = "which-key", delay = 200 }',
    "function M.merge(opts)",
    "  opts = opts or {}",
    "  local out = {}",
    "  for k, v in pairs(M.defaults) do out[k] = v end",
    "  for k, v in pairs(opts) do out[k] = v end",
    "  return out",
    "end",
    "return M",
    "",
  ].join("\n"));
  writeFileSync(join(luaDir, "util.lua"), [
    "local M = {}",
    "function M.label(name, delay)",
    '  return string.format("[%s@%d]", name, delay)',
    "end",
    "return M",
    "",
  ].join("\n"));
  return amalgamate([join(root, "which-key")]);
}

async function writeOpfs(page, name, text) {
  await page.evaluate(async ({ name, text }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(name, { create: true });
    const w = await fh.createWritable();
    await w.write(text);
    await w.close();
  }, { name, text });
}

async function removeOpfs(page, name) {
  await page.evaluate(async (n) => {
    try { (await navigator.storage.getDirectory()).removeEntry(n); } catch {}
  }, name);
}

const BUNDLE = buildBundle();

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

  // First load to get an OPFS scope, then seed the bundle + an init.lua that uses it.
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  await writeOpfs(page, "plugins-bundle.lua", BUNDLE);
  await writeOpfs(page, "init.lua", 'require("which-key").setup({ delay = 50 })\n');

  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // execLua returns a DEBUG-wrapped result, e.g. `ok:String(Utf8String { s: Ok("…") })`.
  // Extract and JSON.parse the inner string literal so the assertions see the raw value.
  const unwrap = (s) => {
    const m = String(s).match(/Ok\((".*")\)\s*}\s*\)\s*$/s);
    if (m) { try { return JSON.parse(m[1]); } catch { return m[1]; } }
    return String(s);
  };
  const luaResult = (code) =>
    page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code).then(unwrap);

  // 1. The plugin loaded from the bundle and its setup() ran at boot: the label composes
  //    the default name (from which-key.config) with the overridden delay, formatted by
  //    which-key.util — proving nested require, defaults merge + override, and the module
  //    return value threading through require.
  const label = String(await luaResult("return _G.__wk_label or 'nil'"));
  check("bundle: require('which-key').setup{} ran at boot (nested require + merge composed)",
    /\[which-key@50\]/.test(label), `label=${label}`);

  // 2. The preload modules persist after boot: a runtime require resolves a submodule.
  const utilOk = String(await luaResult(
    "return require('which-key.util').label('x', 9)"));
  check("bundle: a submodule resolves via require at runtime (preload persists)",
    /\[x@9\]/.test(utilOk), `util=${utilOk}`);

  // 3. A missing module fails loud — no silent stub.
  const missing = String(await luaResult(
    "local ok, err = pcall(require, 'which-key.nonexistent'); return tostring(ok)..'|'..tostring(err)"));
  check("bundle: require of a missing module fails loud",
    /^false\|/.test(missing) && /nonexistent/.test(missing), `missing=${missing}`);

  // 4. Remove the bundle, keep an init.lua that pcall-requires the plugin: it must FAIL —
  //    proving the modules came from the bundle (not an ambient leak) — and boot is
  //    non-fatal (the editor still edits).
  await removeOpfs(page, "plugins-bundle.lua");
  await writeOpfs(page, "init.lua",
    'local ok = pcall(require, "which-key"); vim.g.__wk_loaded = ok and "yes" or "no"\n');
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  const loadedNoBundle = String(await luaResult("return vim.g.__wk_loaded or 'nil'"));
  check("bundle: with the bundle absent, require('which-key') fails (no ambient leak)",
    loadedNoBundle === "no", `loaded=${loadedNoBundle}`);
  await page.evaluate(() => window.__bemtvi.feed("ggdGistill-works<Esc>"));
  const stillEdits = await page.evaluate(() => window.__bemtvi.lines());
  check("bundle: a missing bundle is non-fatal (editor still boots + edits)",
    stillEdits === "still-works", `lines=${JSON.stringify(stillEdits)}`);

  // Clean up OPFS so a re-run starts clean.
  await removeOpfs(page, "init.lua");
  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — multi-file plugin amalgamated to one package.preload bundle, sourced from OPFS, require() resolves (nested), missing fails loud, absent bundle non-fatal"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
