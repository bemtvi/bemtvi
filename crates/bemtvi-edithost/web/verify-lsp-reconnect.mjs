// Playwright verifier for the browser edit-host's LSP leg ACROSS A DAEMON RECONNECT — the
// intersection of verify-lsp.mjs (a real language server running on the daemon, driven by the
// in-Worker `SyncLspClient`) and verify-reconnect.mjs (the Worker's reconnect supervisor
// re-dialing underneath the seams).
//
// What it guards. A server going away must be *retired*, not merely forgotten: its Lua client
// handle dropped from `btv.lsp.clients()`, and its unfinished `$/progress` task dropped with it
// (the `end` is never coming). A dropped link is the hardest case, because the retirement is
// driven by an exit event that no process actually reported — each leg has to synthesize one.
// The native demux drops the `exit_tx` so `RemoteLspProcess::wait` resolves `(None, None)`; the
// Worker pushes a synthetic `lsp_exited` per live server ("a dropped link is a server exit to
// the SyncLspClient", worker.mjs). Those two synthesizers are precisely what keeps
// `resync_lsp_after_reconnect` — the one teardown that drops a server record *without*
// `retire_lsp_server` — from ever meeting a live record. Delete either and the resync silently
// starts forgetting servers instead of retiring them, on one leg only.
//
// The browser leg is the one worth a harness: it is where `SyncLspClient` runs, where the exit
// is synthesized furthest from any process, and where the seams cross a real WebTransport link.
// So, after a drop:
//
//   * the dead pre-drop client is gone from `btv.lsp.clients()` (a leaked handle makes a buffer
//     report two servers where one process is running — `:LspInfo` disagreeing with
//     `btv.lsp.clients()`, exactly the failure `retire_lsp_server`'s doc comment describes);
//   * its unfinished `$/progress` task went with it, rather than being republished under the
//     FRESH client id by the `Initialized` mirror push and spinning forever;
//   * and the respawned server is genuinely live — it re-`didOpen`s its document, so its
//     scripted diagnostics land again on the NEW wire.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi — the daemon AND the mock server), and a Chromium for Playwright.
// Run:  node verify-lsp-reconnect.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8146; // disjoint from verify-lsp (8143) / verify-reconnect (8141) / drop-failures (8144)
const BEMTVI = process.env.BEMTVI_BIN || `${here}../../../target/debug/bemtvi`;

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

// Poll the page until `pred(value)` holds (or timeout), returning the last value.
async function until(page, fn, pred, ms = 10000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Evaluate a Lua expression in the page and return its rendered string. `execLua` renders the
// result in debug form (`…Ok("x")…`), so unwrap the quoted payload.
async function lua(page, expr) {
  const r = await page.evaluate(
    (e) => window.__bemtvi.execLua(`return tostring(${e})`).then((r) => String(r.result)),
    expr,
  );
  const m = r.match(/"((?:[^"\\]|\\.)*)"/);
  return m ? m[1] : r;
}

async function untilLua(page, expr, want, ms = 10000) {
  const start = Date.now();
  for (;;) {
    const v = await lua(page, expr);
    if (v === want) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// The current `btv.daemon.status()` phase. Note "disconnected" *contains* "connected" as a
// substring, so match the whole word.
async function status(page) {
  const r = await page.evaluate(() =>
    window.__bemtvi.execLua("return btv.daemon.status()").then((r) => String(r.result)),
  );
  const m = r.match(/"(connected|reconnecting|disconnected)"/);
  return m ? m[1] : r;
}

async function untilStatus(page, want, ms = 10000) {
  const start = Date.now();
  for (;;) {
    const s = await status(page);
    if (s === want) return s;
    if (Date.now() - start > ms) return s;
    await sleep(40);
  }
}

// The ids `btv.lsp.clients()` lists, sorted — a leaked handle shows as an extra id rather than a
// wrong one, so the ids say *which* client survived and not merely how many.
const CLIENT_IDS = `(function()
  local ids = {}
  for _, c in ipairs(btv.lsp.clients()) do ids[#ids + 1] = c.id end
  table.sort(ids)
  return table.concat(ids, ",")
end)()`;

// Every live `$/progress` task, as `client_id:title`.
const PROGRESS = `(function()
  local out = {}
  for _, p in ipairs(btv.lsp.progress()) do out[#out + 1] = p.client_id .. ":" .. p.title end
  table.sort(out)
  return table.concat(out, ",")
end)()`;

// ── The daemon's project file, and the mock server's script ───────────────────────────────
const root = mkdtempSync(join(tmpdir(), "bemtvi-lsp-reconnect-"));
const file = join(root, "a.rs");
writeFileSync(file, "fn main() {}");
const mockJson = join(root, "mock.json");
// A `begin` with no `end`: the task is still running when the link drops, which is the state
// that strands a spinner. The diagnostic is the liveness probe for the respawned server — it is
// pushed on `didOpen`, so it can only reappear if the fresh process really re-opened the buffer.
writeFileSync(
  mockJson,
  JSON.stringify({
    progress: [{ token: "t1", value: { kind: "begin", title: "Indexing", percentage: 10 } }],
    diagnostics: [
      {
        range: { start: { line: 0, character: 3 }, end: { line: 0, character: 7 } },
        severity: 1,
        message: "diag-from-mock",
      },
    ],
  }),
);

// ── Spawn the real daemon; parse its connect URI from stdout ──────────────────────────────
const daemon = spawn(BEMTVI, ["--daemon", "--listen", "127.0.0.1:0"], {
  stdio: ["ignore", "pipe", "pipe"],
});
let uri = null;
let daemonOut = "";
daemon.stdout.on("data", (d) => {
  daemonOut += d.toString();
  const m = daemonOut.match(/bemtvi:\/\/[^'\s]+/);
  if (m) uri = m[0];
});
daemon.stderr.on("data", (d) => process.stderr.write(`  [daemon] ${d}`));

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => {
  try {
    daemon.kill();
  } catch {}
  try {
    srv.kill();
  } catch {}
};
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 100 && !uri; i++) await sleep(50);
  if (!uri) throw new Error(`daemon never printed a connect URI; stdout=${JSON.stringify(daemonOut)}`);
  console.log("daemon listening:", uri.replace(/\/[0-9a-f]{64}\?/, "/<token>?"));

  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`http://localhost:${PORT}/web/`);
      break;
    } catch {
      await sleep(100);
    }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => {
    if (m.type() === "error") console.log("  [page error]", m.text());
  });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // ── 1. Open the file and start the server on the daemon ────────────────────────────────
  await page.evaluate((f) => window.__bemtvi.feed(":e " + f), file);
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  const opened = await until(page, () => window.__bemtvi.lines(), (v) => v === "fn main() {}");
  check("setup: :e <file>.rs reads the daemon's buffer over the wire", opened === "fn main() {}", `lines=${JSON.stringify(opened)}`);

  await page.evaluate(
    ([bin, script]) =>
      window.__bemtvi.execLua(
        `btv.lsp.config("mock", { cmd = { ${JSON.stringify(bin)}, "--__lsp-mock", ${JSON.stringify(script)} }, filetypes = { "rust" } })
         btv.lsp.enable({ "mock" })`,
      ),
    [BEMTVI, mockJson],
  );

  const before = await untilLua(page, CLIENT_IDS, "1");
  check("setup: the mock server started on the daemon (one client, id 1)", before === "1", `ids=${before}`);
  const busy = await untilLua(page, PROGRESS, "1:Indexing");
  check("setup: the server's $/progress task is live before the drop", busy === "1:Indexing", `progress=${busy}`);

  // ── 2. Drop the link. The remote server child dies with the daemon; the supervisor
  //       re-dials and `resync_lsp_after_reconnect` re-`ensure`s the server on the new one. ──
  await page.evaluate(() => window.__bemtvi.debugDropDaemon());
  const reconnected = await untilStatus(page, "connected");
  check("status: the link auto-recovers to connected", reconnected === "connected", `status=${reconnected}`);

  // ── 3. The respawn mints client id 2. A leaked handle shows up as `1,2` — the dead
  //       pre-drop client listed next to the live one. ──────────────────────────────────────
  const after = await untilLua(page, CLIENT_IDS, "2");
  check(
    "reconnect: the pre-drop client handle is retired (btv.lsp.clients() lists only the respawn)",
    after === "2",
    `ids=${after} — "1,2" means the dead client's handle leaked past the resync`,
  );

  // ── 4. …and its unfinished task went with it. The respawned process reports its own
  //       `begin` under id 2, so the ONLY correct end state is the fresh task alone. ─────────
  const progress = await untilLua(page, PROGRESS, "2:Indexing");
  check(
    "reconnect: only the respawned server's progress is live (the dead task did not survive)",
    progress === "2:Indexing",
    `progress=${progress} — an entry under client 1 is a spinner that outlived its server`,
  );

  // ── 5. Liveness: the respawn genuinely re-`didOpen`ed the buffer on the NEW wire, so its
  //       pushed diagnostic is back. Without this the checks above could pass with a server
  //       that reconnected in name only. ────────────────────────────────────────────────────
  const diags = await untilLua(page, "#btv.diagnostic.get(0)", "1");
  check(
    "reconnect: the respawned server re-didOpen'd the buffer (its pushed diagnostic is back)",
    diags === "1",
    `count=${diags}`,
  );

  await browser.close();
} catch (e) {
  console.error("verify-lsp-reconnect error:", e);
  failures++;
} finally {
  try {
    if (browser) await browser.close();
  } catch {}
  cleanup();
}

console.log(
  failures === 0
    ? "\nALL PASS — the browser edit-host's LSP leg survives a daemon reconnect with no stale client handle and no stranded progress"
    : `\n${failures} FAILED`,
);
process.exit(failures === 0 ? 0 : 1);
