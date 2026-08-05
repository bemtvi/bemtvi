// Playwright verifier for SERVERLESS LSP **hover** rendering — basedpyright `textDocument/hover`
// shown as a syntax-highlighted doc-float, fully in-browser (Phase 4 of
// docs/plans/2026-06-23-web-python-demo.md). Companion to verify-basedpyright-lsp.mjs.
//
// Root-cause guard: nxvim's LSP client did not advertise a `hover.contentFormat`, so pyright /
// basedpyright defaulted to *plaintext* hover — a bare `def f() -> None` with no ```lang fence.
// The hover float renders as a `markdown` buffer whose only highlightable part is a fenced code
// block, so with no fence there was nothing to colour (on web OR native). The client now
// advertises markdown, so the signature comes back fenced (```python … ```).
//
// Faithfulness (not a no-op): we open a python file, hover (K) on a function, and assert
//   1. a `[Hover]` markdown doc-float opens,
//   2. its content carries a ```python fence around the inferred signature (the capability fix),
//   3. the rendered float paints more than one foreground colour over that fenced code (the
//      client-side fence highlighter actually coloured it).
//
// Runs against the **python-demo** site (build-demo.sh → demo-site/). Prereqs: ./build-demo.sh
// and a Chromium for Playwright (PW_CHROMIUM on macOS). Run: node verify-basedpyright-hover-highlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8157;

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
async function until(page, fn, pred, ms = 45000) {
  const start = Date.now();
  let v;
  for (;;) { v = await page.evaluate(fn); if (pred(v)) return v; if (Date.now() - start > ms) return v; await sleep(150); }
}
const luaResult = (page, code) => page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

const DEMO_SITE = `${here}../demo-site`;
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit", env: { ...process.env, NXVIM_SERVE_ROOT: DEMO_SITE },
});
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) { try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); } }
  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted serverless (window.__nxvim.ready resolved, no daemon)", true);

  const PYCODE = "def add(a: int, b: int) -> int:\n    return a + b\n\nresult = add(2, 3)\n";
  await page.evaluate(async (text) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("hov.py", { create: true });
    const ws = await fh.createWritable(); await ws.write(text); await ws.close();
  }, PYCODE);
  await page.evaluate(() => window.__nxvim.feed(":e /hov.py<CR>"));
  await until(page, () => window.__nxvim.lines(), (v) => /def add/.test(String(v)));
  check("setup: :e /hov.py opens the python buffer", true);

  await luaResult(page, `
    nx.lsp.config("basedpyright", {
      cmd = { "basedpyright-langserver", "--stdio" }, filetypes = { "python" },
      settings = { basedpyright = { analysis = { typeshedPaths = { "/typeshed" }, typeCheckingMode = "basic" } } },
    })
    nx.lsp.enable("basedpyright")
    return 1`);
  await until(page, () => window.__nxvim.execLua("return tostring(#nx.lsp.clients())").then((r) => r.result), (v) => /[1-9]/.test(String(v)));
  await sleep(1500);

  // Place the cursor on the `add` definition name and hover (K).
  await luaResult(page, `
    local b = vim.api.nvim_get_current_buf()
    local ls = vim.api.nvim_buf_get_lines(b, 0, -1, false)
    for i, l in ipairs(ls) do
      local c = l:find("def add")
      if c then nx.cursor.set({ i, c + 3 }); break end
    end
    return 1`);
  await sleep(200);
  await page.evaluate(() => window.__nxvim.feed("K"));

  // 1. A markdown doc-float opens.
  const hov = await until(page, () => {
    const w = ((window.__nxvim.frame() || {}).windows || []).find((x) => /Hover/.test(String(x.file_name)));
    return w ? { filetype: w.filetype, floating: !!w.floating } : null;
  }, (v) => v != null);
  check("hover: K opens a [Hover] markdown doc-float", hov != null && hov.filetype === "markdown" && hov.floating, JSON.stringify(hov));

  // 2. Its content is a ```python-fenced signature (the contentFormat capability fix). Read it
  //    straight from the hover buffer's lines.
  const hoverText = await luaResult(page, `
    for _, b in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_get_name(b):find("Hover") then
        return table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), "\\n")
      end
    end
    return ""`);
  const ht = String(hoverText);
  check("hover: the content is markdown-fenced ```python (not plaintext)", /```python/.test(ht) && /def add/.test(ht), `hover=${JSON.stringify(ht)}`);

  // 3. The rendered float actually colours the fenced code (more than one fg colour over the
  //    floating window's cells). The hover float is a non-focused floating window; its cells
  //    sit in #grid. We scope to spans whose text is part of the signature keywords/types.
  await sleep(400);
  const colorInfo = await page.evaluate(() => {
    const spans = Array.from(document.querySelectorAll("#grid span"));
    // Keywords/types of the fenced signature that should each carry their own colour.
    const wanted = ["def", "int", "None", "->"];
    const hits = {};
    for (const el of spans) {
      const t = (el.textContent || "").trim();
      if (wanted.includes(t)) hits[t + "@" + getComputedStyle(el).color] = (hits[t + "@" + getComputedStyle(el).color] || 0) + 1;
    }
    const colors = new Set(Object.keys(hits).map((k) => k.split("@")[1]));
    return { keys: Object.keys(hits), distinctColors: colors.size };
  });
  check("hover: the fenced signature is syntax-highlighted (keywords/types carry distinct colours)",
    colorInfo.distinctColors >= 2, JSON.stringify(colorInfo));

  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  await browser.close();
} catch (e) {
  console.error("verify-basedpyright-hover-highlight error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless basedpyright hover: markdown-fenced signature, syntax-highlighted in-browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
