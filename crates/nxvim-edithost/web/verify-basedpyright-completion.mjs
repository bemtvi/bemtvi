// Playwright verifier for SERVERLESS LSP **autocompletion** — basedpyright completions
// flowing into the native `nx.complete` popup, fully in-browser (Phase 4 of
// docs/plans/2026-06-23-web-python-demo.md). Companion to verify-basedpyright-lsp.mjs
// (diagnostics + hover); this one drives the `textDocument/completion` round-trip through
// the engine's `lsp` source.
//
// Faithfulness (not a no-op): a python buffer `value = math` is opened, basedpyright is
// enabled, and in insert mode we type `.sqr` after `math`. The `.` is a trigger char; the
// engine issues `textDocument/completion` to basedpyright, which — only because it resolved
// typeshed and knows `math` is the stdlib module — answers with member completions. We
// assert the popup carries `sqrt`, a symbol that appears NOWHERE in the buffer text, so it
// can only come from the language server (a buffer word-scan could never produce it).
//
// This guards the regression where the `lsp` completion source was `#[cfg(feature =
// "native")]`-gated out of the wasm edit-host: completion silently fell back to buffer
// words only, with no LSP items — "lsp autocompletion not enabled" on the web demo.
//
// Runs against the **python-demo** site (build-demo.sh → demo-site/). Prereqs: ./build-demo.sh
// and a Chromium for Playwright (PW_CHROMIUM on macOS). Run: node verify-basedpyright-completion.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8153;

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

async function until(page, fn, pred, ms = 45000) {
  const start = Date.now();
  let v;
  for (;;) {
    v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(150);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

const DEMO_SITE = `${here}../demo-site`;
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, NXVIM_SERVE_ROOT: DEMO_SITE },
});
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // SERVERLESS: no `?daemon=` — there is no backend at all.
  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted serverless (window.__nxvim.ready resolved, no daemon)", true);

  // A python buffer whose completion target — `math.<member>` — is NOT spelled anywhere
  // in the text, so any `sqrt` in the popup proves an LSP answer, not a buffer word-scan.
  const PYCODE = "import math\nvalue = math\n";
  await page.evaluate(async (text) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("comp.py", { create: true });
    const ws = await fh.createWritable();
    await ws.write(text);
    await ws.close();
  }, PYCODE);

  await page.evaluate(() => window.__nxvim.feed(":e /comp.py<CR>"));
  const opened = await until(page, () => window.__nxvim.lines(), (v) => /value = math/.test(String(v)));
  check("setup: :e /comp.py opens the python buffer", /value = math/.test(String(opened)), `lines=${JSON.stringify(opened)}`);

  // Configure + enable basedpyright (the local host routes the spawn to the bundled worker)
  // and the native completion engine with the `lsp` source leading. `min_chars = 1` opens
  // the popup eagerly; `.` is basedpyright's member-access trigger char (it advertises it in
  // its completion capabilities, which the engine folds into its trigger set).
  await luaResult(page, `
    nx.lsp.config("basedpyright", {
      cmd = { "basedpyright-langserver", "--stdio" },
      filetypes = { "python" },
      settings = { basedpyright = { analysis = {
        typeshedPaths = { "/typeshed" },
        typeCheckingMode = "basic",
        diagnosticMode = "openFilesOnly",
      } } },
    })
    nx.lsp.enable("basedpyright")
    nx.complete.setup({ sources = { { "lsp" }, { "buffer", min_chars = 2 } }, min_chars = 1 })
    return 1`);

  // Wait until basedpyright has attached + analyzed the buffer (it lists a client). Without
  // this the first completion can race the server's startup and come back empty.
  const clients = await until(
    page,
    () => window.__nxvim.execLua("return tostring(#nx.lsp.clients())").then((r) => r.result),
    (v) => /[1-9]/.test(String(v)),
  );
  check("lsp: basedpyright attached to the python buffer", /[1-9]/.test(String(clients)), `clients=${JSON.stringify(clients)}`);
  // Give the server a beat to finish its first analysis pass so member completions resolve.
  await sleep(1500);

  // Insert mode at end of `value = math`, then type the trigger char `.` and a `sqr` prefix.
  // The engine issues `textDocument/completion`; basedpyright answers `math`'s members, the
  // prefix narrows them, and the popup fills.
  await page.evaluate(() => window.__nxvim.feed("GA"));
  await sleep(150);
  await page.evaluate(() => window.__nxvim.feed("."));
  await sleep(400);
  await page.evaluate(() => window.__nxvim.feed("sqr"));

  // The popup is open and carries `sqrt` — an LSP-only completion (not in the buffer text).
  const menu = await until(
    page,
    () => {
      const m = (window.__nxvim.frame() || {}).menu || null;
      return m ? { items: m.items || [], n: (m.items || []).length } : null;
    },
    (v) => v != null && v.items.some((it) => /\bsqrt\b/.test(String(it))),
  );
  const items = menu ? menu.items : [];
  check("complete: the popup opened with LSP items (basedpyright)", menu != null && menu.n > 0, `menu=${JSON.stringify(menu)}`);
  check("complete: `sqrt` (an LSP-only member of `math`, absent from the buffer) is offered",
    items.some((it) => /\bsqrt\b/.test(String(it))), `items=${JSON.stringify(items)}`);

  // Auto-triggered popups open noselect (nvim-cmp style), so highlight the row with <C-n>
  // first. Selecting an `lsp` row also fires its lazy `completionItem/resolve`, filling the
  // docs sidebar beside the popup.
  await page.evaluate(() => window.__nxvim.feed("<C-n>"));
  await sleep(1200);

  // The docs sidebar: basedpyright sends the item's signature as a ```python-fenced markdown
  // block, which must (a) reach the web build at all — it was `#[cfg(native)]`-gated out —
  // and (b) be syntax-highlighted client-side (the wasm build ships no spans for floats, so
  // the renderer fence-highlights it). Assert both: the docs lines carry a python fence, and
  // the rendered `.pmenu-doc` box paints more than one foreground colour.
  const docs = await until(
    page,
    () => { const m = (window.__nxvim.frame() || {}).menu; return m && m.docs ? m.docs.lines : null; },
    (v) => v != null && v.some((l) => /```python/.test(String(l))),
  );
  check("complete docs: the sidebar shows the item's ```python-fenced signature",
    Array.isArray(docs) && docs.some((l) => /```python/.test(String(l))) && docs.some((l) => /def sqrt/.test(String(l))),
    `docs=${JSON.stringify(docs)}`);
  const docColors = await page.evaluate(() => {
    const cells = Array.from(document.querySelectorAll(".pmenu-doc span"));
    const colors = new Set(cells.map((el) => getComputedStyle(el).color));
    return { spanCount: cells.length, distinct: colors.size };
  });
  check("complete docs: the fenced signature is syntax-highlighted (multiple colours in .pmenu-doc)",
    docColors.spanCount > 0 && docColors.distinct >= 2, JSON.stringify(docColors));

  // Accept the highlighted row (<C-y>) and confirm the buffer now reads `value = math.sqrt`
  // — the chosen item's edit was applied through the server-native accept path.
  await page.evaluate(() => window.__nxvim.feed("<C-y>"));
  const accepted = await until(
    page,
    () => window.__nxvim.lines(),
    (v) => /value = math\.sqrt/.test(String(v)),
  );
  check("complete: accepting the row inserts `math.sqrt` into the buffer",
    /value = math\.sqrt/.test(String(accepted)), `lines=${JSON.stringify(accepted)}`);

  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await browser.close();
} catch (e) {
  console.error("verify-basedpyright-completion error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless basedpyright autocompletion: real LSP items in the nx.complete popup, no daemon"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
