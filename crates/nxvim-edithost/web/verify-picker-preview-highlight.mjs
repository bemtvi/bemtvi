// Playwright verifier for SYNTAX HIGHLIGHTING in the picker PREVIEW pane on the pure web
// client (serverless, no daemon). The picker's `files`/`live_grep` preview shows a slice of
// the selected file; on the pure-wasm build the server has no native tree-sitter engine, so
// it ships EMPTY `preview.highlights`. Window text is highlighted client-side via
// web-tree-sitter (spansForWindow); the bug was that the preview pane never took that path,
// so previews rendered plain while the file open in a window highlighted fine.
//
// This seeds a `.rs` file (rust is a BUNDLED grammar — highlights offline, no :TSInstall),
// opens the `files` picker, selects the file, and asserts the preview pane's DOM carries
// colored spans (`#grid .pmenu .row span[style*="color"]`).
//
// Faithfulness (not a no-op): the file is seeded through the real nx.fs/OPFS seam, the real
// `files` picker runs its production async source + preview fetch, and the assertion reads
// the live rendered preview DOM — not a stub.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-preview-highlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8147;

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
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__nxvim.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Poll the picker's live candidate list until the seeded file shows up (or timeout) — the
// serverless `files` source walks OPFS asynchronously, so the list fills in over a tick or two.
async function pollPickerHasFile(page, name, ms = 8000) {
  const start = Date.now();
  let last = "";
  for (;;) {
    last = String(await luaResult(page,
      `local p = nx._picker\n` +
      `if not p then return "NOPICKER" end\n` +
      `local t = {}\n` +
      `for i = 1, (p.nitems or 0) do t[#t + 1] = p.items[i].text end\n` +
      `return table.concat(t, "\\n")`));
    if (new RegExp(name.replace(/\./g, "\\.")).test(last)) return last;
    if (Date.now() - start > ms) return last;
    await sleep(60);
  }
}

// Poll the rendered preview pane until it carries colored spans (or timeout). The picker
// preview pane is the SECOND `.pmenu` box (the list is the first); read every colored span
// across the popup so a layout tweak doesn't break the selector.
async function waitPreviewColored(page, ms = 8000) {
  const start = Date.now();
  let detail = "";
  for (;;) {
    const r = await page.evaluate(() => {
      const spans = [...document.querySelectorAll("#grid .pmenu .row span[style]")];
      const styled = spans.filter((s) => /color\s*:/.test(s.getAttribute("style")));
      return { any: styled.length, sample: styled.slice(0, 6).map((s) => s.textContent) };
    });
    if (r.any > 0) return { ok: true, detail: JSON.stringify(r.sample) };
    detail = JSON.stringify(r);
    if (Date.now() - start > ms) return { ok: false, detail };
    await sleep(80);
  }
}

const ROOT_REL = `pickprev-${Date.now()}`;

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

  // No `?daemon=` — serverless, so the server ships no syntax spans for the preview.
  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless — no daemon, no native treesitter engine)", true);

  // Seed a rust file (bundled grammar → highlights offline, no :TSInstall) under the cwd.
  // Quote-free source on purpose: the body is embedded into a Lua string literal below, so a
  // `"` in the source would terminate it and the seed would silently fail. `fn`/`let` + the
  // function name + the numbers give the grammar plenty to colour.
  const src = [
    "fn main() {",
    "    let x = 42;",
    "    let y = x + 1;",
    "}",
  ].join("\\n");
  const seeded = await settle(page, "__seed", `nx.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       nx.await(nx.fs.mkdir(base, { recursive = true }))
       nx.await(nx.fs.write(base .. "/demo.rs", "${src}\\n"))
       _G.__seed = "ok"
     end)()`);
  // Match the SEEDED value specifically — `/ok/` alone false-passes on the rmpv `Ok(…)` wrapper.
  check("seeded demo.rs under cwd via nx.fs (OPFS)", /Ok\("ok"\)/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  // Open the real `files` picker; the OPFS walk fills the candidate list asynchronously, so
  // wait until the seeded file shows up before narrowing to it.
  await luaResult(page, `nx.picker.open('files')`);
  const items = await pollPickerHasFile(page, "demo.rs");
  check("file picker lists the seeded demo.rs (nx.fs walk)", /demo\.rs/.test(items), `items=${JSON.stringify(items)}`);

  // Narrow to the seeded file so it's the selected row — its preview pane then renders the
  // file slice (fetched off-tick over the OPFS fs seam).
  await page.evaluate(() => window.__nxvim.feed("demo.rs"));

  // The preview pane must show colored (tree-sitter) spans — the heart of the fix.
  const colored = await waitPreviewColored(page);
  check("picker preview pane is syntax-highlighted client-side (rust bundled grammar)", colored.ok, colored.detail);

  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await settle(page, "__rm", `nx.fs.remove(vim.fn.getcwd() .. "/${ROOT_REL}", { recursive = true })
       :next(function() _G.__rm = "gone" end, function(e) _G.__rm = "err:" .. e.code end)`);
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — picker preview pane is syntax-highlighted on the pure-wasm build"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
