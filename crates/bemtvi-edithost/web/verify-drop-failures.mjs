// Playwright verifier: a dropped daemon link must FAIL the in-flight async work loudly —
// the browser mirror of the native teardown contract (daemon.rs: `run_demux` synthesizes a
// `-1` proc exit "so the editor's one-shot on_exit always fires and is never leaked";
// `run_lsp_demux` drops every exit_tx "rather than hanging"). Before the fix, the Worker's
// `clearDaemonLiveState()` silently forgot the live ids, so an in-flight `btv.run` promise,
// `btv.process` on_exit, and `btv.socket` on_close all hung forever across a link drop — a
// silent stub, violating CLAUDE.md "fail loud".
//
// Flow: spawn a real `bemtvi --daemon --listen` and a local TCP echo peer, open the page
// with `?daemon=<uri>`, start one in-flight leg of each kind (a long-running `btv.run`
// child, a duplex `btv.process` child on `cat`, an `btv.socket` connection), force the link
// to drop (window.__bemtvi.debugDropDaemon), and assert each callback fires promptly with
// the loud failure (-1 exit / close error) — and that the link still auto-recovers after.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi), and a Chromium for Playwright. Run:  node verify-drop-failures.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8144; // http server (verify-reconnect uses 8141; keep disjoint)
const TCP_PORT = 8155; // the btv.socket peer
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
async function until(page, fn, pred, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Render a Lua chunk's result via execLua ("ok:…" / "err:…").
async function lua(page, code) {
  return page.evaluate(
    (c) => window.__bemtvi.execLua(c).then((r) => String(r.result)),
    code,
  );
}

// Extract the returned Lua string from execLua's rendered debug form
// (`ok:String(Utf8String { s: Ok("…") })`), or null on an error / non-string result.
function luaStr(rendered) {
  if (!rendered.startsWith("ok:")) return null;
  const m = rendered.match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1] : null;
}

async function status(page) {
  const r = await lua(page, "return btv.daemon.status()");
  const m = r.match(/"(connected|reconnecting|disconnected)"/);
  return m ? m[1] : r;
}

async function untilStatus(page, want, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const s = await status(page);
    if (s === want) return s;
    if (Date.now() - start > ms) return s;
    await sleep(40);
  }
}

// ── A local TCP peer for btv.socket (the daemon dials it; never sends, never closes) ──────
const tcp = createServer((sock) => sock.on("error", () => {}));
tcp.listen(TCP_PORT, "127.0.0.1");

// ── Spawn the real daemon; parse its connect URI from stdout ──────────────────────────────
const daemon = spawn(BEMTVI, ["--daemon", "--listen", "127.0.0.1:0"], { stdio: ["ignore", "pipe", "pipe"] });
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
  try { daemon.kill(); } catch {}
  try { srv.kill(); } catch {}
  try { tcp.close(); } catch {}
};
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

  await page.goto(`http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("status: a fresh daemon session reports connected", (await untilStatus(page, "connected")) === "connected");

  // ── 1. Put one long-lived job in flight on each leg ─────────────────────────────────────
  const setup = await lua(
    page,
    `
    _G.__proc, _G.__dproc, _G.__sock, _G.__sock_up = nil, nil, nil, nil
    -- one-shot proc leg: a child that outlives the link (btv.run resolves on exit only)
    btv.run({ cmd = "sleep", args = { "60" } }):next(function(r) _G.__proc = r.code end)
    -- duplex proc leg: cat blocks on stdin forever
    btv.process.open({ cmd = "cat", on_exit = function(code) _G.__dproc = code end })
    -- socket leg: a connection to the verifier's TCP peer (on the daemon's host)
    btv.socket.connect({
      host = "127.0.0.1", port = ${TCP_PORT},
      on_connect = function() _G.__sock_up = true end,
      on_close = function(err) _G.__sock = tostring(err or "clean") end,
    })
    return "armed"
    `,
  );
  check("setup: the three legs armed without error", luaStr(setup) === "armed", `result=${setup}`);
  const sockUp = await until(page, () => window.__bemtvi.execLua("return _G.__sock_up").then((r) => String(r.result)), (v) => v.includes("true"));
  check("socket: the TCP connection established before the drop", sockUp.includes("true"), `sock_up=${sockUp}`);

  // ── 2. Drop the link: every in-flight callback must fire loudly, promptly ───────────────
  await page.evaluate(() => window.__bemtvi.debugDropDaemon());
  const state = await until(
    page,
    () =>
      window.__bemtvi
        .execLua("return tostring(_G.__proc) .. '|' .. tostring(_G.__dproc) .. '|' .. tostring(_G.__sock)")
        .then((r) => String(r.result)),
    (v) => !v.includes("nil"),
  );
  const [proc, dproc, sock] = (luaStr(state) ?? state).split("|");
  check("proc: the in-flight btv.run on_exit fired with a -1 exit (never leaked)", proc === "-1", `state=${state}`);
  check("dproc: the duplex btv.process on_exit fired with a -1 exit", dproc === "-1", `state=${state}`);
  check(
    "sock: the btv.socket on_close fired with the loud connection error",
    typeof sock === "string" && sock.includes("daemon connection"),
    `state=${state}`,
  );

  // ── 3. The failure is a teardown, not a wedge: the supervisor still auto-recovers ───────
  const recovered = await untilStatus(page, "connected");
  check("status: the link still auto-recovers to connected after the failures", recovered === "connected", `status=${recovered}`);

  await browser.close();
} catch (e) {
  console.error("verify-drop-failures error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — a dropped daemon link fails in-flight proc/dproc/sock work loudly and recovers"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
