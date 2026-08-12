// Playwright verifier: the picker's MIDDLE SEPARATOR (the vertical `│` strip dividing the
// list column from the preview pane) must track the colorscheme's background, not a hardcoded
// dark chrome colour. Bug: `appendGlyphVRule` created a `.popup-chrome` strip and never set its
// background, so under a LIGHT theme the list + preview panes themed light while the separator
// between them stayed dark (`.popup-chrome { background:#21252b }`).
//
// This sets a LIGHT `TelescopeNormal` (the first group in the picker's `bg` fallback chain),
// opens the `files` picker WITH a preview pane, and asserts the separator strip's computed
// background equals the list box's themed light background — and is NOT the dark chrome default.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-separator-theme.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8151;
const LIGHT = "rgb(238, 238, 238)"; // #eeeeee — the light picker bg we set below
const DARK = "rgb(33, 37, 43)"; // #21252b — the hardcoded .popup-chrome default (the bug)

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

const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__bemtvi.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

async function pollPickerHasFile(page, name, ms = 8000) {
  const start = Date.now();
  let last = "";
  for (;;) {
    last = String(await luaResult(page,
      `local p = btv._picker\n` +
      `if not p then return "NOPICKER" end\n` +
      `local t = {}\n` +
      `for i = 1, (p.nitems or 0) do t[#t + 1] = p.items[i].text end\n` +
      `return table.concat(t, "\\n")`));
    if (new RegExp(name.replace(/\./g, "\\.")).test(last)) return last;
    if (Date.now() - start > ms) return last;
    await sleep(60);
  }
}

// Read the middle separator strip + the list box backgrounds from the live DOM. The separator
// is the `.popup-chrome` whose text is ONLY `│` glyphs (the outer ring also is `.popup-chrome`
// but carries corners/─; the list/preview panes are `.pmenu`). Poll until it appears.
async function readSeparatorVsBox(page, ms = 8000) {
  const start = Date.now();
  let detail = "";
  for (;;) {
    const r = await page.evaluate(() => {
      const chromes = [...document.querySelectorAll("#grid .popup-chrome")];
      const vrule = chromes.find((c) => {
        const t = (c.textContent || "").replace(/\s/g, "");
        return t.length > 0 && [...t].every((ch) => ch === "│");
      });
      const box = document.querySelector("#grid .pmenu");
      if (!vrule || !box) return { ready: false };
      return {
        ready: true,
        sep: getComputedStyle(vrule).backgroundColor,
        box: getComputedStyle(box).backgroundColor,
      };
    });
    if (r.ready) return r;
    detail = JSON.stringify(r);
    if (Date.now() - start > ms) return { ready: false, detail };
    await sleep(80);
  }
}

const ROOT_REL = `picksep-${Date.now()}`;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 1100, height: 700 } });
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted (serverless)", true);

  // A LIGHT picker background: TelescopeNormal is the first group in the picker `bg` chain, so
  // this makes the box + preview + (post-fix) separator resolve to #eeeeee. Also give the border
  // a visible colour so the resolve path is exercised.
  await settle(page, "__hl", `btv.schedule(function()
       vim.api.nvim_set_hl(0, "TelescopeNormal", { bg = "#eeeeee", fg = "#111111" })
       vim.api.nvim_set_hl(0, "TelescopeBorder", { fg = "#333333" })
       _G.__hl = "set"
     end)`);
  check("set a light TelescopeNormal", /Ok\("set"\)/.test(String(await luaResult(page, "return tostring(_G.__hl)"))));

  // Seed a file so the `files` picker has a candidate whose preview pane (and thus the middle
  // separator) renders.
  const src = ["fn main() {", "    let x = 42;", "}"].join("\\n");
  const seeded = await settle(page, "__seed", `btv.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       btv.await(btv.fs.mkdir(base, { recursive = true }))
       btv.await(btv.fs.write(base .. "/demo.rs", "${src}\\n"))
       _G.__seed = "ok"
     end)()`);
  check("seeded demo.rs under cwd (OPFS)", /Ok\("ok"\)/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  await luaResult(page, `btv.picker.open('files')`);
  const items = await pollPickerHasFile(page, "demo.rs");
  check("files picker lists demo.rs", /demo\.rs/.test(items), `items=${JSON.stringify(items)}`);
  await page.evaluate(() => window.__bemtvi.feed("demo.rs"));

  const r = await readSeparatorVsBox(page);
  check("picker preview + middle separator rendered", r.ready, r.detail);
  check("middle separator bg matches the themed light box bg (not hardcoded dark)",
    r.ready && r.sep === r.box && r.sep === LIGHT && r.sep !== DARK,
    `separator=${r.sep} box=${r.box} (want ${LIGHT}, bug=${DARK})`);

  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await settle(page, "__rm", `btv.fs.remove(vim.fn.getcwd() .. "/${ROOT_REL}", { recursive = true })
       :next(function() _G.__rm = "gone" end, function(e) _G.__rm = "err:" .. e.code end)`);
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — the picker's middle separator tracks the themed background"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
