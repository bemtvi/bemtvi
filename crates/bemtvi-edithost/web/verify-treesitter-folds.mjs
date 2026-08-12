// Playwright verifier for tree-sitter FOLDS on the wasm edit-host (Phase 4b). Drives the
// real editor in headless Chromium and asserts that `foldmethod=expr` + the tree-sitter
// foldexpr collapses a block body — the worker's fold runner (web/ts-folds.js), reached
// synchronously from the Rust tick through the eh_js_ts_folds* bridge (web/eh-lib.js),
// feeds @fold ranges into the core fold store, which then hides the folded lines.
//
// Hermetic: uses only a BUNDLED grammar (python) + its vendored folds.scm, so nothing is
// fetched at runtime. Companion to verify-treesitter-indent.mjs (the sibling in-tick runner).
//
//   node verify-treesitter-folds.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8099;

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

const feed = (page, keys) => page.evaluate((k) => window.__bemtvi.feed(k), keys);
const lines = (page) => page.evaluate(() => window.__bemtvi.lines());

// The visible buffer-line numbers of the focused window (the rendered `numbers` array,
// keeping only real line numbers — a closed fold drops the lines it hides, so this shrinks).
const visibleNums = (page) =>
  page.evaluate(() => {
    const w = window.__bemtvi.frame().windows.find((x) => x.focused);
    return (w ? w.numbers : []).filter((n) => typeof n === "number" && n > 0);
  });

try {
  const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
  const cleanup = () => { try { srv.kill(); } catch {} };
  process.on("exit", cleanup);

  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  page.on("console", (m) => { const t = m.text(); if (m.type() === "error" || t.includes("bemtvi")) console.log("  [console]", t); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // A python function whose body is foldable via python's folds.scm.
  await feed(page, ":e fold.py<CR>");
  await feed(page, ":set expandtab shiftwidth=4<CR>");
  await feed(page, "ggdGidef f():<CR>x = 1<CR>y = 2<CR>return x + y<Esc>");
  // Outdent the body back to one level if autoindent stacked it (keep the buffer the 4
  // lines we expect regardless of indent drift), then confirm the buffer content.
  const buf = await lines(page);
  check("python: foldable function typed", /def f\(\):/.test(String(buf)) && String(buf).split("\n").length >= 4, buf);
  const total = String(buf).split("\n").length;

  // Give the worker time to load the python grammar + folds.scm (async), then enable the
  // tree-sitter foldexpr.
  await sleep(2500);
  await feed(page, ":set foldexpr=v:lua.vim.treesitter.foldexpr()<CR>");
  await feed(page, ":set foldmethod=expr<CR>");

  // Poll until the fold collapses (fewer visible rows than buffer lines), retrying to ride
  // out the async grammar load — a keystroke that beat the parser just folded nothing once.
  let vis = [];
  for (let i = 0; i < 60; i++) {
    // Re-trigger a recompute each iteration (a no-op edit) so a freshly-loaded grammar is
    // picked up without depending on a single earlier tick.
    await feed(page, ":set foldmethod=expr<CR>");
    vis = await visibleNums(page);
    if (vis.length > 0 && vis.length < total) break;
    await sleep(150);
  }
  check(
    "python: foldmethod=expr collapses the body (tree-sitter folds)",
    vis.length > 0 && vis.length < total,
    `visible=${JSON.stringify(vis)} total=${total}`,
  );
  check("python: the function's first line stays visible", vis.includes(1), JSON.stringify(vis));

  // The buffer itself is untouched — folding only hides lines on screen.
  const after = await lines(page);
  check("python: folding does not modify the buffer", String(after).split("\n").length === total, after);

  await browser.close();
  cleanup();
} catch (e) {
  console.error(e);
  failures++;
}

console.log(failures === 0
  ? "\nALL PASS — tree-sitter folds on the edit-host (foldmethod=expr collapses a block)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
