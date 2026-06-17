// Playwright verifier for the browser edit-host's LSP leg (Phase 6e) against a REAL
// `nxvim --daemon --listen` over WebTransport. The browser has no process host, so a
// language server can't run locally — `vim.lsp.start` crosses the wire: the in-Worker
// `SyncLspClient`'s `lsp_spawn`/`lsp_stdin`/`lsp_kill` go to the daemon, which runs the
// real server (`serve_one_lsp`, the same wire the native `RemoteLspTransport` uses), and
// the server's `lsp_stdout`/`lsp_stderr`/`lsp_exited` return as pushes the Worker feeds
// back into the client. The browser twin of the native `lsp_config.rs` / `lsp_float.rs`
// black-box tests.
//
// The scripted mock server (`nxvim --__lsp-mock <json>`, `nxvim_lsp::mock`) is what the
// daemon spawns — a real child on the daemon's machine, configured verbatim via the
// config's `cmd` (the browser has no `$NXVIM_LSP_CMD` env hook; `lsp_spawn` falls through
// to the config cmd). Faithfulness (not a no-op):
//   1. diagnostics round-trip: the mock pushes `textDocument/publishDiagnostics` on
//      `didOpen` — a SERVER→client push that only a real `didOpen` over the wire triggers
//      — and the scripted message lands in `nx.diagnostic.get()`'s queryable editor state.
//   2. hover round-trip: `nx.lsp.hover()` fires a request whose reply opens the content
//      float with the scripted markup (a request/reply round-trip the opposite direction).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim — the daemon AND the mock server), and a Chromium for Playwright.
// Run:  node verify-lsp.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8143;
const NXVIM = process.env.NXVIM_BIN || `${here}../../../target/debug/nxvim`;

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

// Poll the page until `pred(value)` holds (or timeout), returning the last value.
async function until(page, fn, pred, ms = 10000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(50);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// ── The daemon's working tree (real disk; the .rs file + the mock script live here) ────────
const root = mkdtempSync(join(tmpdir(), "nxvim-lsp-"));
const rsFile = join(root, "a.rs");
writeFileSync(rsFile, "let foo = bar()\n");
// The scripted mock: diagnostics pushed on didOpen + a hover reply. Cargo.toml-style
// absolute paths so the daemon (same machine) can spawn it and read the script.
const mockJson = join(root, "mock.json");
writeFileSync(
  mockJson,
  JSON.stringify({
    diagnostics: [
      {
        range: { start: { line: 0, character: 4 }, end: { line: 0, character: 7 } },
        severity: 1,
        message: "scripted-diag-from-daemon",
      },
    ],
    hover: { contents: { kind: "markdown", value: "`foo`: a scripted hover symbol" } },
  }),
);

// ── Spawn the real daemon; parse its connect URI from stdout ──────────────────────────────
const daemon = spawn(NXVIM, ["--daemon", "--listen", "127.0.0.1:0"], { stdio: ["ignore", "pipe", "pipe"] });
let uri = null;
let daemonOut = "";
daemon.stdout.on("data", (d) => {
  daemonOut += d.toString();
  const m = daemonOut.match(/nxvim:\/\/[^'\s]+/);
  if (m) uri = m[0];
});
daemon.stderr.on("data", (d) => process.stderr.write(`  [daemon] ${d}`));

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { daemon.kill(); } catch {} try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 100 && !uri; i++) await sleep(50);
  if (!uri) throw new Error(`daemon never printed a connect URI; stdout=${JSON.stringify(daemonOut)}`);
  console.log("daemon listening:", uri.replace(/\/[0-9a-f]{64}\?/, "/<token>?"));

  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  const pageUrl = `http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`;
  await page.goto(pageUrl);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted + dialed the daemon (window.__nxvim.ready resolved)", true);

  // ── Open the .rs buffer over the wire (filetype rust, `foo` under the cursor) ───────────
  await page.evaluate((f) => window.__nxvim.feed(":e " + f), rsFile);
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  const opened = await until(page, () => window.__nxvim.lines(), (v) => /let foo = bar\(\)/.test(String(v)));
  check("setup: :e <file>.rs reads the daemon's buffer over the wire", /let foo = bar\(\)/.test(String(opened)), `lines=${JSON.stringify(opened)}`);
  // Cursor on `foo` so a hover request has a symbol (and the reply's cursor-staleness gate passes).
  await page.evaluate(() => window.__nxvim.feed("0fw"));

  // ── Declare + enable the mock server. `enable` processes the already-open buffer's
  //    FileType (rust), so the SyncLspClient spawns the server on the daemon over the wire. ──
  await luaResult(
    page,
    `nx.lsp.config("mock", { cmd = { ${JSON.stringify(NXVIM)}, "--__lsp-mock", ${JSON.stringify(mockJson)} }, filetypes = { "rust" } })
     nx.lsp.enable("mock")
     return 1`,
  );

  // ── 1. The server started on the daemon (it appears in nx.lsp.clients) ──────────────────
  const clients = await until(
    page,
    () => window.__nxvim.execLua("return tostring(#nx.lsp.clients())").then((r) => r.result),
    (v) => /[1-9]/.test(String(v)),
  );
  check("lsp: the mock server started on the daemon (nx.lsp.clients() lists it)", /[1-9]/.test(String(clients)), `clients=${JSON.stringify(clients)}`);

  // ── 2. diagnostics round-trip: didOpen → server push → nx.diagnostic.get() editor state ──
  const diags = await until(
    page,
    () =>
      window.__nxvim
        .execLua(
          `local ds = nx.diagnostic.get(0) or {}
           local msgs = {}
           for _, d in ipairs(ds) do msgs[#msgs+1] = d.message end
           return table.concat(msgs, "|")`,
        )
        .then((r) => r.result),
    (v) => /scripted-diag-from-daemon/.test(String(v)),
  );
  check("lsp: server-pushed diagnostics (didOpen → publishDiagnostics) land in nx.diagnostic.get() over the wire",
    /scripted-diag-from-daemon/.test(String(diags)), `diags=${JSON.stringify(diags)}`);

  // ── 3. hover round-trip: nx.lsp.hover() request → reply opens the content float ─────────
  const hoverText = await until(
    page,
    () => {
      window.__nxvim.execLua("nx.lsp.hover()");
      const f = window.__nxvim.frame() && window.__nxvim.frame().float;
      if (!f || !Array.isArray(f.lines)) return "";
      // Each float line is a chunk run `[[text, style_id], …]`; concatenate the chunk texts.
      return f.lines
        .map((row) => (Array.isArray(row) ? row.map((c) => (Array.isArray(c) ? c[0] : "")).join("") : ""))
        .join("\n");
    },
    (v) => /scripted hover symbol/.test(String(v)),
  );
  check("lsp: nx.lsp.hover() reply opens the content float with the server's markup (request/reply over the wire)",
    /scripted hover symbol/.test(String(hoverText)), `float=${JSON.stringify(hoverText)}`);

  await browser.close();
} catch (e) {
  console.error("verify-lsp error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host runs an LSP server on a real nxvim --daemon over WebTransport (diagnostics push + hover reply)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
