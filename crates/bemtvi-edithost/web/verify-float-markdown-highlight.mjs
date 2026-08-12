// Playwright verifier for SYNTAX HIGHLIGHTING in a markdown doc-float on the pure web
// client (serverless). LSP hover / signature help render into a cursor-anchored doc-float
// window (`Editor::open_doc_float`) typed `markdown`. The markdown grammar isn't bundled and
// web-tree-sitter doesn't run tree-sitter injections, so the code inside a ```lang fence —
// a hover's signature, the part that matters — rendered PLAIN on web. The fix highlights the
// fenced code with its own (bundled) grammar.
//
// This reproduces the hover shape with `btv.view`: a markdown float carrying a ```rust fence,
// then moves focus back to the main window so the float is non-focused (exactly like a real
// hover, where focus stays in the editing window). It asserts the fenced rust tokens get
// colored spans, while a prose line stays plain.
//
// Faithfulness (not a no-op): the float is a real non-focused floating window over a typed
// scratch buffer (the same surface hover/signature use), its text reaches the client over the
// real `eh_aux_lines` background-text seam, and the assertion reads the live rendered DOM.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-float-markdown-highlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8148;

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

// Poll the floating window's DOM until the fenced rust code carries colored spans (or timeout),
// reading the run texts so we can assert WHICH tokens colored.
async function waitFloatColored(page, ms = 8000) {
  const start = Date.now();
  let detail = "";
  for (;;) {
    const r = await page.evaluate(() => {
      const out = [];
      for (const f of document.querySelectorAll("#grid .win")) {
        const txt = [...f.querySelectorAll(".row")].map((r) => r.textContent).join("|");
        if (!/fn helper|let y/.test(txt)) continue;
        out.push([...f.querySelectorAll('span[style*="color"]')].map((s) => s.textContent));
      }
      return out.flat();
    });
    if (r.length > 0) return { ok: true, colored: r };
    detail = JSON.stringify(r);
    if (Date.now() - start > ms) return { ok: false, detail };
    await sleep(80);
  }
}

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
  check("Worker booted (serverless — no native treesitter engine)", true);

  // Open a rust file first so the bundled rust grammar is loaded and ready (a real hover
  // is requested over a code buffer, so its fence language's grammar is already warm).
  await page.evaluate(() => window.__bemtvi.feed(":e demo.rs<CR>ggdGifn main() { let z = 99; }<Esc>"));
  await sleep(700);

  // A markdown doc-float carrying a ```rust fence (the hover shape), then unfocus it so it's
  // a passive popup like a real hover (focus stays in the editing window).
  const mounted = await luaResult(page, `
    _G.v = btv.view.create({ filetype = "markdown" })
    _G.v:set_lines({ "# helper", "", "\`\`\`rust", "fn helper(x: i32) -> i32 {", "    let y = x + 1;", "    y", "}", "\`\`\`", "", "Returns the incremented value." })
    _G.v:mount({ float = { relative = "editor", anchor = "NW", row = 2, col = 2, width = 46, height = 12, border = "rounded", grab = false } })
    return "mounted"`);
  check("markdown doc-float mounted (hover shape)", /Ok\("mounted"\)/.test(String(mounted)), `r=${JSON.stringify(mounted)}`);
  await page.evaluate(() => window.__bemtvi.feed("<C-w>w")); // unfocus the float — like a real hover

  const colored = await waitFloatColored(page);
  check("fenced rust code in the markdown hover is syntax-highlighted client-side", colored.ok, colored.detail);
  if (colored.ok) {
    // The rust keywords inside the fence must be among the colored runs (the code is what
    // matters); a prose word like "Returns" must NOT be colored (no markdown grammar bundled).
    const set = new Set(colored.colored.map((s) => String(s).trim()));
    check("rust keywords inside the fence colored (fn / let)", set.has("fn") && set.has("let"), JSON.stringify([...set]));
    check("prose outside the fence stays plain (no markdown grammar)", !set.has("Returns"), JSON.stringify([...set]));
  }
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — markdown doc-float (hover) highlights its fenced code on the pure-wasm build"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
