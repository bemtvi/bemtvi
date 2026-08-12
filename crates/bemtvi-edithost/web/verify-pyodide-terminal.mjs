// Playwright verifier for the LOCAL (serverless) `:terminal python …` leg — the in-browser
// python interpreter (Pyodide) that runs with NO daemon and NO server (Phase 1 of
// docs/plans/2026-06-23-web-python-demo.md). The browser has no PTY and no process; here the
// process IS python compiled to wasm, running in a sibling Web Worker (web/pyodide-worker.mjs),
// its stdout streaming back through the same terminal seam the daemon leg uses.
//
// Faithfulness (not a no-op): the page is opened SERVERLESS (no `?daemon=`), a python file is
// written to OPFS, and `:terminal python /demo.py` runs it — producing computed output
// (`sqrt(1764)` → 42) that only a real interpreter could, rendered into the terminal buffer, then
// a clean `[Process exited 0]`. No wire, no daemon: a static page running CPython.
//
// This runs against the **python-demo** site (build-demo.sh → demo-site/), NOT the standard
// editor — the local Pyodide host is only installed there (build-config localHost:true). The
// verify serves demo-site/ via BEMTVI_SERVE_ROOT.
//
// Prereqs: ./build-demo.sh (assembles demo-site/ with Pyodide + the demo build-config), and a
// Chromium for Playwright (PW_CHROMIUM=/path/to/chrome on macOS). Run: node verify-pyodide-terminal.mjs
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

const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

// The whole terminal buffer as one string (history + live screen), for substring assertions.
const READ_BUF = 'return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\\n")';

// Poll the terminal buffer until `re` appears — returns the matched state, or the last buffer
// text on timeout so a failure shows what *did* render. The first python run includes Pyodide's
// one-time load (wasm instantiate + stdlib unzip), so the default budget is generous.
async function pollBuf(page, re, ms = 40000) {
  const start = Date.now();
  let last = "";
  for (;;) {
    last = String(await luaResult(page, READ_BUF));
    if (re.test(last)) return last;
    if (Date.now() - start > ms) return last;
    await sleep(100);
  }
}

// Read an OPFS file from the page (to confirm the `:w` flushed before python reads it).
const opfsRead = (page, name) =>
  page.evaluate(async (n) => {
    try {
      const root = await navigator.storage.getDirectory();
      const fh = await root.getFileHandle(n);
      return await (await fh.getFile()).text();
    } catch {
      return null;
    }
  }, name);

// Serve the assembled demo site (web/ + dist/ + Pyodide, build-config localHost:true).
const DEMO_SITE = `${here}../demo-site`;
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, BEMTVI_SERVE_ROOT: DEMO_SITE },
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

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (serverless, SAB transport)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted serverless (window.__bemtvi.ready resolved, no daemon)", true);

  // The gate is open even with no daemon: the local process host advertises a process host.
  // (`vim.system` would now enqueue; the terminal leg is what Phase 1 fulfils.)
  // Write a real python program to OPFS — flat (no indentation) so no autoindent intervenes.
  // Type into a FRESH buffer: the demo build seeds + opens TOUR.md, whose content loads from
  // OPFS asynchronously — `:enew` gives a clean [No Name] the pending read can't land in.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>:enew<CR>"));
  await page.evaluate(() => window.__bemtvi.feed("ifrom math import sqrt<CR>"));
  await page.evaluate(() => window.__bemtvi.feed('print("PYRESULT", int(sqrt(1764)))<CR>'));
  await page.evaluate(() => window.__bemtvi.feed('print("interpreter-ok")<Esc>'));
  await page.evaluate(() => window.__bemtvi.feed(":w /demo.py<CR>"));

  // Wait for the OPFS write to flush before python mounts + reads it.
  let saved = null;
  for (let i = 0; i < 50 && saved == null; i++) { saved = await opfsRead(page, "demo.py"); await sleep(100); }
  check("setup: the python file was written to OPFS", saved != null && /PYRESULT/.test(saved || ""), `demo.py=${JSON.stringify(saved)}`);

  // ── Run it in the in-browser interpreter. `:terminal python /demo.py` → argv ["python",
  // "/demo.py"]; the Pyodide Worker loads (first time), mounts OPFS at /project, and runs the
  // file as __main__, streaming its stdout back into the terminal buffer. ──────────────────────
  await page.evaluate(() => window.__bemtvi.feed(":terminal python /demo.py<CR>"));

  const out = await pollBuf(page, /PYRESULT 42/);
  check("terminal: the in-browser python interpreter computed and printed sqrt(1764)=42",
    /PYRESULT 42/.test(out), `buf=${JSON.stringify(out)}`);
  check("terminal: a second print line streamed too (multi-line stdout)",
    /interpreter-ok/.test(out), `buf=${JSON.stringify(out)}`);

  // The script ran to completion: a clean exit notice renders (term_exit → [Process exited 0]).
  const exited = await pollBuf(page, /\[Process exited 0\]/);
  check("terminal: the script's clean exit renders ([Process exited 0])",
    /\[Process exited 0\]/.test(exited), `buf=${JSON.stringify(exited)}`);

  await browser.close();
} catch (e) {
  console.error("verify-pyodide-terminal error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless `:terminal python` runs CPython (Pyodide) in-browser, output rendered, clean exit"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
