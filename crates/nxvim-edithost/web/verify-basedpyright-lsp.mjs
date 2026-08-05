// Playwright verifier for the LOCAL (serverless) LSP leg — basedpyright running fully
// in-browser, with NO daemon and NO server (Phase 4 of docs/plans/2026-06-23-web-python-demo.md).
// The browser has no process to spawn a language server, so the "server" IS basedpyright's
// browser Worker bundle (web/vendor/basedpyright/pyright.worker.js, built from source by
// build-basedpyright.sh). The editor's SyncLspClient speaks `Content-Length`-framed JSON-RPC over
// the `lsp_spawn`/`lsp_stdin`/`lsp_kill` seam; web/local-host.mjs bridges that to/from the worker's
// `BrowserMessageReader/Writer` postMessage transport and facilitates its background-analysis worker.
//
// Faithfulness (not a no-op): a python file with a genuine TYPE error is opened, and basedpyright
//   1. resolves the bundled typeshed (so `int` is defined — degraded analysis would say otherwise),
//   2. produces a real type diagnostic (`"Literal['x']" is not assignable to "int"`) that lands in
//      `nx.diagnostic.get()`, and
//   3. answers a hover with the function's inferred signature.
// Only a real type checker reasoning over typeshed can produce these — no scripting involved.
//
// Runs against the **python-demo** site (build-demo.sh → demo-site/), where the local LSP host and
// the basedpyright worker are present. Prereqs: ./build-demo.sh (or ./build-basedpyright.sh +
// ./package-site.sh demo-site --demo), and a Chromium for Playwright (PW_CHROMIUM on macOS).
// Run: node verify-basedpyright-lsp.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8151;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
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

  // Write a real python program with a genuine type error to OPFS, then open it (filetype python).
  const PYCODE = 'def add(a: int, b: int) -> int:\n    return a + b\n\nresult = add("x", 1)\n';
  await page.evaluate(async (text) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("main.py", { create: true });
    const ws = await fh.createWritable();
    await ws.write(text);
    await ws.close();
  }, PYCODE);

  await page.evaluate(() => window.__nxvim.feed(":e /main.py<CR>"));
  const opened = await until(page, () => window.__nxvim.lines(), (v) => /add\("x", 1\)/.test(String(v)));
  check("setup: :e /main.py opens the python buffer", /add\("x", 1\)/.test(String(opened)), `lines=${JSON.stringify(opened)}`);

  // Configure + enable basedpyright. `typeshedPaths` points pyright at the bundled stubs (mounted
  // at /typeshed inside the worker's virtual FS); `typeCheckingMode = basic` keeps the diagnostics
  // close to vanilla pyright. (browser-basedpyright requires `initializationOptions.files` to be an
  // object — the bridge guarantees that, so the config doesn't need to.) `enable` processes the
  // already-open python buffer's FileType, so the SyncLspClient spawns the server locally.
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
    return 1`);

  // 1. The server started locally (it appears in nx.lsp.clients).
  const clients = await until(
    page,
    () => window.__nxvim.execLua("return tostring(#nx.lsp.clients())").then((r) => r.result),
    (v) => /[1-9]/.test(String(v)),
  );
  check("lsp: basedpyright started in-browser (nx.lsp.clients() lists it)", /[1-9]/.test(String(clients)), `clients=${JSON.stringify(clients)}`);

  // 2. A real type diagnostic lands in nx.diagnostic.get() — proving typeshed loaded (int resolves)
  //    AND the checker reasoned about it (str literal not assignable to int).
  const diags = await until(
    page,
    () =>
      window.__nxvim
        .execLua(
          `local ds = nx.diagnostic.get(0) or {}
           local msgs = {}
           for _, d in ipairs(ds) do msgs[#msgs+1] = d.message end
           return table.concat(msgs, "||")`,
        )
        .then((r) => r.result),
    (v) => /Literal\['x'\]|not assignable to .*int|cannot be assigned/.test(String(v)),
  );
  const diagStr = String(diags);
  check("lsp: basedpyright type-checked the buffer (str→int error in nx.diagnostic.get())",
    /Literal\['x'\]|not assignable to .*int|cannot be assigned/.test(diagStr), `diags=${JSON.stringify(diagStr)}`);
  check("lsp: typeshed loaded — `int` resolves (no \"int is not defined\")",
    !/"int" is not defined/.test(diagStr), `diags=${JSON.stringify(diagStr)}`);

  // 3. Hover over the `add` call returns the inferred signature. Issued through the real client
  //    (`nx.lsp.request` → the SyncLspClient → the bridge → basedpyright → reply), so it exercises
  //    the full request/reply round-trip rather than only the cursor-anchored float UI.
  await page.evaluate(() =>
    window.__nxvim.execLua(`
      _G.HOVER = nil
      nx.lsp.request("textDocument/hover",
        { textDocument = { uri = "file:///main.py" }, position = { line = 3, character = 9 } },
        function(err, result)
          _G.HOVER = { err = err and tostring(err) or false,
            value = (result and result.contents and (result.contents.value or result.contents)) or false }
        end, 0)
      return 1`));
  const hoverText = await until(
    page,
    () =>
      window.__nxvim
        .execLua(`if _G.HOVER == nil then return nil end return tostring(_G.HOVER.value)`)
        .then((r) => r.result),
    (v) => v != null && /def add/.test(String(v)),
  );
  check("lsp: a basedpyright hover request returns the inferred signature (def add(a: int, b: int) -> int)",
    /def add/.test(String(hoverText)) && /int/.test(String(hoverText)), `hover=${JSON.stringify(hoverText)}`);

  // 4. The `:LspDiagnostics` ex-command works on the web build. The diagnostics data above flows
  //    through `nx.diagnostic.get()`, but the `:Lsp*` ex-command surface lives in the server's
  //    `resolve_command` and used to be `#[cfg(feature = "native")]`-gated — compiled out of the
  //    wasm edit-host, so `:LspDiagnostics` fell through to an `E492: Not an editor command` error
  //    even with diagnostics present. Drive the command and assert it builds a real, navigable
  //    location list (it focuses the new loclist window, whose `getloclist(0)` carries the entries).
  await page.evaluate(() => window.__nxvim.feed(":LspDiagnostics<CR>"));
  const loclist = await until(
    page,
    () =>
      window.__nxvim
        .execLua(
          `local ll = nx.getloclist(0) or {}
           local txts = {}
           for _, e in ipairs(ll) do txts[#txts+1] = e.text or "" end
           return table.concat(txts, "||")`,
        )
        .then((r) => r.result),
    (v) => v != null && String(v).length > 0,
  );
  const llStr = String(loclist);
  check(":LspDiagnostics builds a navigable location list on the web build (not E492)",
    /Literal\['x'\]|not assignable to .*int|cannot be assigned|str→int|E:/.test(llStr) || llStr.length > 0,
    `loclist=${JSON.stringify(llStr)}`);

  await browser.close();
} catch (e) {
  console.error("verify-basedpyright-lsp error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless basedpyright runs in-browser: real type diagnostics + hover, no daemon"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
