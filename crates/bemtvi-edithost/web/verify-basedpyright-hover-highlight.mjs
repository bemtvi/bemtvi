// Playwright verifier for SERVERLESS LSP **hover** rendering — basedpyright `textDocument/hover`
// shown as a syntax-highlighted doc-float, fully in-browser (Phase 4 of
// docs/plans/2026-06-23-web-python-demo.md). Companion to verify-basedpyright-lsp.mjs.
//
// Root-cause guard: bemtvi's LSP client did not advertise a `hover.contentFormat`, so pyright /
// basedpyright defaulted to *plaintext* hover — a bare `def f() -> None` with no ```lang fence.
// The hover float renders as a `markdown` buffer whose only highlightable part is a fenced code
// block, so with no fence there was nothing to colour (on web OR native). The client now
// advertises markdown, so the signature comes back fenced (```python … ```).
//
// The fence is guarded ON THE WIRE, not in the float's buffer. When this test was written the
// hover float WAS a `markdown` buffer holding the raw reply, so asserting ```python over its
// lines was the same thing. It no longer is: doc floats now render markdown through
// `bemtvi-core/markdown.rs` (`Editor::open_markdown_float`), which strips the markup to display
// lines + `@markup.*` extmark spans and deliberately leaves the buffer UNTYPED so its own
// filetype pass can't repaint the stripped text. So a correct, markdown-fenced hover shows up in
// the buffer with no fence and with `filetype == ""` — which is why the reply itself is what we
// assert on.
//
// Faithfulness (not a no-op): we open a python file, hover (K) on a function, and assert
//   1. the RAW reply is `kind: "markdown"` and carries a ```python fence (the capability fix —
//      this is the regression the file exists for, checked before any rendering),
//   2. a `[Hover]` doc-float opens and holds the RENDERED signature: `def add` present, fence
//      markers consumed (proving the markdown renderer ran rather than dumping raw markup),
//   3. the float paints more than one foreground colour over that fenced code (the client-side
//      fence highlighter actually coloured it), scoped to the float's own cells.
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
const luaResult = (page, code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

const DEMO_SITE = `${here}../demo-site`;
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit", env: { ...process.env, BEMTVI_SERVE_ROOT: DEMO_SITE },
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
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted serverless (window.__bemtvi.ready resolved, no daemon)", true);

  const PYCODE = "def add(a: int, b: int) -> int:\n    return a + b\n\nresult = add(2, 3)\n";
  await page.evaluate(async (text) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("hov.py", { create: true });
    const ws = await fh.createWritable(); await ws.write(text); await ws.close();
  }, PYCODE);
  await page.evaluate(() => window.__bemtvi.feed(":e /hov.py<CR>"));
  await until(page, () => window.__bemtvi.lines(), (v) => /def add/.test(String(v)));
  check("setup: :e /hov.py opens the python buffer", true);

  await luaResult(page, `
    btv.lsp.config("basedpyright", {
      cmd = { "basedpyright-langserver", "--stdio" }, filetypes = { "python" },
      settings = { basedpyright = { analysis = { typeshedPaths = { "/typeshed" }, typeCheckingMode = "basic" } } },
    })
    btv.lsp.enable("basedpyright")
    return 1`);
  await until(page, () => window.__bemtvi.execLua("return tostring(#btv.lsp.clients())").then((r) => r.result), (v) => /[1-9]/.test(String(v)));
  await sleep(1500);

  // 1. The RAW reply carries the markdown fence. Issued through the real client before `K`,
  //    while the PYTHON buffer is still current — `btv.lsp.request` routes on bufnr 0, and after
  //    `K` that is the (client-less) float buffer, where the request would never be sent.
  await page.evaluate(() => window.__bemtvi.execLua(`
    _G.HOVER_RAW = nil
    btv.lsp.request("textDocument/hover",
      { textDocument = { uri = "file:///hov.py" }, position = { line = 0, character = 5 } },
      function(err, result)
        local c = result and result.contents
        _G.HOVER_RAW = { kind = type(c) == "table" and tostring(c.kind) or "<not-markup-content>",
                         value = type(c) == "table" and tostring(c.value) or tostring(c) }
      end, 0)
    return 1`));
  // Read the two fields separately and match over each reply as a whole: `execLua` hands
  // back a DEBUG-FORMATTED wrapper (`ok:String(Utf8String { s: Ok("…") })`), not the bare
  // Lua string, so splitting it on a separator recovers nothing — every other verifier
  // here regexes the wrapper, and so does this.
  // Polled with `luaResult` rather than `until`: `until` serializes its callback to the
  // page, so a closure over the field name would arrive with nothing bound to it.
  const rawField = async (f) => {
    for (let i = 0; i < 200; i++) {
      const v = await luaResult(page,
        `if _G.HOVER_RAW == nil then return nil end return _G.HOVER_RAW.${f}`);
      if (v != null && /Ok\(/.test(String(v))) return v;
      await sleep(150);
    }
    return null;
  };
  const rawKind = String(await rawField("kind"));
  const rawValue = String(await rawField("value"));
  check("hover: the reply is markdown-fenced ```python (contentFormat advertised)",
    /markdown/.test(rawKind) && /```python/.test(rawValue) && /def add/.test(rawValue),
    `kind=${JSON.stringify(rawKind)} value=${JSON.stringify(rawValue)}`);

  // Place the cursor on the `add` definition name and hover (K).
  await luaResult(page, `
    local b = vim.api.nvim_get_current_buf()
    local ls = vim.api.nvim_buf_get_lines(b, 0, -1, false)
    for i, l in ipairs(ls) do
      local c = l:find("def add")
      if c then btv.cursor.set({ i, c + 3 }); break end
    end
    return 1`);
  await sleep(200);
  await page.evaluate(() => window.__bemtvi.feed("K"));

  // 2. A `[Hover]` doc-float opens, holding the RENDERED markdown: the signature is there and
  //    the fence markers are gone (consumed by the renderer, not passed through as text).
  const hov = await until(page, () => {
    const w = ((window.__bemtvi.frame() || {}).windows || []).find((x) => /Hover/.test(String(x.file_name)));
    return w ? { filetype: w.filetype, floating: !!w.floating } : null;
  }, (v) => v != null);
  check("hover: K opens a [Hover] doc-float", hov != null && hov.floating, JSON.stringify(hov));

  const hoverText = await luaResult(page, `
    for _, b in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_get_name(b):find("Hover") then
        return table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), "\\n")
      end
    end
    return ""`);
  const ht = String(hoverText);
  check("hover: the float holds the rendered signature, fence stripped",
    /def add/.test(ht) && !/```/.test(ht), `hover=${JSON.stringify(ht)}`);

  // 3. The float actually colours the fenced code (more than one fg colour over ITS cells).
  //    Scoped to the float geometrically: the float's content spans are appended to #grid as
  //    siblings of its `.float-win` chrome box, not as children of it, so ancestry can't scope
  //    this — but the chrome box's rect can. Unscoped, this check is a no-op: hov.py itself
  //    contains `def add(a: int, b: int) -> int`, so the SOURCE buffer's own tree-sitter colours
  //    would satisfy it even with the float empty or absent.
  await sleep(400);
  const colorInfo = await page.evaluate(() => {
    const box = document.querySelector(".float-win");
    if (!box) return { error: "no .float-win chrome box", distinctColors: 0 };
    const r = box.getBoundingClientRect();
    const inside = (el) => {
      const b = el.getBoundingClientRect();
      const cx = b.left + b.width / 2, cy = b.top + b.height / 2;
      return cx >= r.left && cx <= r.right && cy >= r.top && cy <= r.bottom;
    };
    const spans = Array.from(document.querySelectorAll("#grid span")).filter(inside);
    // Keywords/types of the fenced signature that should each carry their own colour.
    const wanted = ["def", "int", "None", "->"];
    const hits = {};
    for (const el of spans) {
      const t = (el.textContent || "").trim();
      if (wanted.includes(t)) hits[t + "@" + getComputedStyle(el).color] = (hits[t + "@" + getComputedStyle(el).color] || 0) + 1;
    }
    const colors = new Set(Object.keys(hits).map((k) => k.split("@")[1]));
    return { floatSpans: spans.length, keys: Object.keys(hits), distinctColors: colors.size };
  });
  check("hover: the fenced signature is syntax-highlighted (keywords/types carry distinct colours)",
    colorInfo.distinctColors >= 2, JSON.stringify(colorInfo));

  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
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
