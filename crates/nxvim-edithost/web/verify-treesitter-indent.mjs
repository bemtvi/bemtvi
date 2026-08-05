// Playwright verifier for tree-sitter INDENTATION on the wasm edit-host. Drives the real
// editor in headless Chromium and asserts that the worker's web-tree-sitter indenter
// (web/ts-indent.js), reached synchronously from the Rust tick through the eh_js_ts_*
// bridge (web/eh-lib.js), produces nvim-treesitter indentation for:
//   1. insert-mode <CR> after a block opener (python `def f():`, rust `fn f() {`),
//   2. the `=` operator reindenting a flattened buffer.
//
// Hermetic: uses only BUNDLED grammars (python, rust) + their vendored indents.scm, so
// nothing is fetched at runtime. Companion to verify-treesitter.mjs (the install path).
//
//   node verify-treesitter-indent.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8098;

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

const feed = (page, keys) => page.evaluate((k) => window.__nxvim.feed(k), keys);
const lines = (page) => page.evaluate(() => window.__nxvim.lines());

// Open `file` (sets the filetype) and give the worker time to load that grammar — the
// indenter loads grammars asynchronously, so warm it before asserting.
async function open(page, file) {
  await feed(page, `:e ${file}<CR>`);
  // expandtab / shiftwidth are buffer-local, so set them on THIS buffer (spaces, width 4).
  await feed(page, ":set expandtab<CR>");
  await feed(page, ":set shiftwidth=4<CR>");
  await sleep(2500);
}

// Run `keys` (after clearing the buffer with `ggdG` + `prep`) and poll until the resulting
// buffer equals `want`, retrying to ride out any remaining async grammar load (a keystroke
// that beats the parser just falls back that once).
async function expectAfter(page, prep, keys, want) {
  let got = "";
  for (let i = 0; i < 60; i++) {
    await feed(page, "ggdG" + prep + keys);
    got = await lines(page);
    if (got === want) return { ok: true, detail: got };
    await sleep(150);
  }
  return { ok: false, detail: JSON.stringify(got) + " (wanted " + JSON.stringify(want) + ")" };
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
  page.on("console", (m) => { const t = m.text(); if (m.type() === "error" || t.includes("nxvim-indent")) console.log("  [console]", t); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // ---- 1. python: <CR> after `def f():` indents the block body ----
  await open(page, "demo.py");
  const py = await expectAfter(page, "i", "def f():<CR>x = 1<Esc>", "def f():\n    x = 1");
  check("python: <CR> after `def f():` indents the body (ts-indent)", py.ok, py.detail);

  // ---- 2. rust: `o` inside a `{ }` block opens an indented line ----
  // (Mirrors native's `o_opens_an_indented_line_inside_a_block`: a balanced block, so
  // tree-sitter parses it cleanly — `o` on the `{` line opens an indented body line.)
  await open(page, "demo.rs");
  const rs = await expectAfter(page, "ifn f() {<CR>}<Esc>", "ggolet x = 1;<Esc>", "fn f() {\n    let x = 1;\n}");
  check("rust: `o` inside a `{ }` block indents the new line (ts-indent)", rs.ok, rs.detail);

  // ---- 3. rust: the `=` operator reindents a flattened buffer ----
  // Type the (auto-indented) block, flatten every line to column 0 (`:%s/^\s*//` — flat rust
  // is still valid, so the parse recovers), then `gg=G` to reindent through the `=` operator.
  const eq = await expectAfter(
    page,
    "ifn g() {<CR>let y = 2;<CR>}<Esc>:%s/^\\s*//<CR>",
    "gg=G",
    "fn g() {\n    let y = 2;\n}",
  );
  check("rust: `gg=G` reindents a flattened buffer (ts-indent `=`)", eq.ok, eq.detail);

  // ---- 4. without ts-indent a non-grammar buffer still opens at column 0 (no regression) ----
  await open(page, "notes.txt");
  const plain = await expectAfter(page, "i", "hello<CR>world<Esc>", "hello\nworld");
  check("plain text: <CR> does not indent (no grammar → column 0)", plain.ok, plain.detail);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — tree-sitter indentation on the edit-host (autoindent <CR> + `=` operator)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
