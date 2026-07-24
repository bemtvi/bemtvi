// Playwright verifier for tree-sitter TEXT OBJECTS on the wasm edit-host (Phase 4).
// Drives the real editor in headless Chromium and asserts that `daf` (delete around a
// function) and `dia` (delete an argument) select syntactic ranges — the worker's
// text-object runner (web/ts-textobjects.js), reached synchronously from the Rust tick
// through the eh_js_ts_textobjects* bridge (web/eh-lib.js → WasmSyntax in src/lib.rs),
// answers the object query the core's resolve_text_object then applies.
//
// Hermetic: uses only a BUNDLED grammar (python) + its vendored textobjects.scm, so
// nothing is fetched at runtime. Sibling of verify-treesitter-folds.mjs.
//
//   node verify-treesitter-textobjects.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8102;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`),
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

// Type a fresh python function into the current buffer (clearing it first).
async function typeFunction(page) {
  await feed(page, "ggdG");
  await feed(page, "idef add(alpha, beta):<CR>return alpha + beta<Esc>");
}

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
  page.on("console", (m) => { const t = m.text(); if (m.type() === "error" || t.includes("nxvim")) console.log("  [console]", t); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  await feed(page, ":e to.py<CR>");
  await feed(page, ":set expandtab shiftwidth=4<CR>");
  await typeFunction(page);
  const buf = String(await lines(page));
  check("python: function typed", /def add\(/.test(buf) && /return alpha \+ beta/.test(buf), buf);

  // Give the worker time to load the python grammar + textobjects.scm (async).
  await sleep(2500);

  // `daf` (delete around function) from inside the body. Retry to ride out the async
  // grammar load — a keystroke that beats the parser deletes nothing that once, and the
  // buffer still holds the function, so we simply try again.
  let afOk = false, afBuf = buf;
  for (let i = 0; i < 60; i++) {
    await feed(page, "gg/return<CR>"); // park the cursor inside the function body
    await feed(page, "daf");
    afBuf = String(await lines(page));
    if (!/def add/.test(afBuf) && !/return alpha/.test(afBuf)) { afOk = true; break; }
    await sleep(150);
  }
  check("python: `daf` deletes the whole function (tree-sitter text object)", afOk, afBuf);

  // `dia` (delete an argument): retype, park on the signature's `beta`, delete just that
  // parameter. The grammar is already loaded (daf worked), so no retry loop is needed.
  // `beta` legitimately remains in the body (`return alpha + beta`), so the assertion is
  // scoped to the SIGNATURE line: it loses `beta` but keeps `alpha`.
  await typeFunction(page);
  await sleep(300);
  await feed(page, "gg/beta<CR>"); // the first `beta` is the signature parameter
  await feed(page, "dia");
  const iaBuf = String(await lines(page));
  const sig = iaBuf.split("\n")[0];
  check(
    "python: `dia` deletes just the argument",
    /def add\(/.test(sig) && /alpha/.test(sig) && !/beta/.test(sig),
    iaBuf,
  );

  // JavaScript: its textobjects.scm is inherits-only (`; inherits: ecma,jsx`), so this
  // ONLY works if the inherit chain was merged (ecma supplies the actual @function
  // patterns) — the regression guard for inherit merging on the web bundle.
  await feed(page, ":e to.js<CR>");
  await feed(page, ":set expandtab shiftwidth=2<CR>");
  await feed(page, "ggdG");
  await feed(page, "ifunction greet(name) {<CR>return 'hi ' + name;<CR>}<Esc>");
  await sleep(2500);
  let jsOk = false, jsBuf = "";
  for (let i = 0; i < 60; i++) {
    await feed(page, "gg/return<CR>");
    await feed(page, "daf");
    jsBuf = String(await lines(page));
    if (!/function greet/.test(jsBuf) && !/return 'hi/.test(jsBuf)) { jsOk = true; break; }
    await sleep(150);
  }
  check("javascript: `daf` works via merged `ecma` inherits", jsOk, jsBuf);

  await browser.close();
  cleanup();
} catch (e) {
  console.error(e);
  failures++;
}

console.log(failures === 0
  ? "\nALL PASS — tree-sitter text objects on the edit-host (daf / dia select syntactic ranges)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
