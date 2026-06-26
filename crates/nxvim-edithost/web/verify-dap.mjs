// Playwright verifier for the browser edit-host's DUPLEX-process leg (`dproc_*`) and SOCKET
// leg (`sock_*`) — the DAP / framed-protocol transports — against a REAL `nxvim --daemon
// --listen` over WebTransport. The browser has no local process / TCP, so `nx.process.open`
// and `nx.socket.connect` cross the wire to the daemon, which runs the child / dials the
// socket and streams raw bytes both ways. The browser twin of daemon_dproc.rs.
//
// Faithfulness (not a no-op): (1) a `cat` child run ON THE DAEMON echoes stdin → stdout, so
// the bytes written from the browser come back to on_stdout over the wire (duplex); (2) a
// kill from the browser fires on_exit; (3) nx.socket connects — over the wire — to a TCP echo
// server this harness runs, and the bytes it writes round-trip to on_data.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-dap.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";
import { createServer } from "node:net";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8142;
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

async function until(page, fn, pred, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// ── A TCP echo server the daemon dials for the socket test (ephemeral port) ────────────────
const echo = createServer((sock) => sock.on("data", (d) => sock.write(d)));
await new Promise((res) => echo.listen(0, "127.0.0.1", res));
const echoPort = echo.address().port;

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
const cleanup = () => {
  try { daemon.kill(); } catch {}
  try { srv.kill(); } catch {}
  try { echo.close(); } catch {}
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
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted + dialed the daemon", true);

  // ── 1. nx.process duplex: a `cat` child on the daemon echoes the browser's stdin ─────────
  await luaResult(
    page,
    `_G.__got, _G.__exit = "", nil
     _G.__p = nx.process.open({
       cmd = "cat",
       on_stdout = function(c) _G.__got = _G.__got .. c end,
       on_exit = function(code) _G.__exit = code end,
     })
     _G.__p:write("ping-over-webtransport\\n")
     return 1`,
  );
  const got = await until(
    page,
    () => window.__nxvim.execLua("return _G.__got or ''").then((r) => r.result),
    (v) => /ping-over-webtransport/.test(String(v)),
  );
  check("dproc: nx.process stdin round-trips to stdout via a daemon child (duplex over the wire)",
    /ping-over-webtransport/.test(String(got)), `got=${JSON.stringify(got)}`);

  // kill → on_exit fires (the child waits on EOF, so a fire proves the kill crossed the wire)
  await luaResult(page, "_G.__p:kill() return 1");
  const exited = await until(
    page,
    () => window.__nxvim.execLua("return tostring(_G.__exit)").then((r) => r.result),
    (v) => !/nil/.test(String(v)), 8000,
  );
  check("dproc: handle:kill() fires on_exit (kill crossed the wire)", !/nil/.test(String(exited)),
    `exit=${JSON.stringify(exited)}`);

  // ── 2. nx.socket: connect over the wire to the harness's TCP echo server ──────────────────
  await luaResult(
    page,
    `_G.__sdata, _G.__sconn, _G.__sclosed = "", false, nil
     _G.__s = nx.socket.connect({
       host = "127.0.0.1", port = ${echoPort},
       on_connect = function() _G.__sconn = true; _G.__s:write("sock-hi") end,
       on_data = function(d) _G.__sdata = _G.__sdata .. d end,
       on_close = function(e) _G.__sclosed = e or "clean" end,
     })
     return 1`,
  );
  const sconn = await until(
    page,
    () => window.__nxvim.execLua("return _G.__sconn and 1 or 0").then((r) => r.result),
    (v) => /1$/.test(String(v)),
  );
  check("sock: nx.socket connected to the echo server (over the wire)", /1$/.test(String(sconn)),
    `conn=${JSON.stringify(sconn)}`);
  const sdata = await until(
    page,
    () => window.__nxvim.execLua("return _G.__sdata or ''").then((r) => r.result),
    (v) => /sock-hi/.test(String(v)),
  );
  check("sock: bytes written round-trip back to on_data (duplex TCP over the wire)",
    /sock-hi/.test(String(sdata)), `data=${JSON.stringify(sdata)}`);

  await luaResult(page, "_G.__s:close() return 1");
  const sclosed = await until(
    page,
    () => window.__nxvim.execLua("return tostring(_G.__sclosed)").then((r) => r.result),
    (v) => !/nil/.test(String(v)), 5000,
  );
  check("sock: close() fires on_close", !/nil/.test(String(sclosed)), `closed=${JSON.stringify(sclosed)}`);

  await browser.close();
} catch (e) {
  console.error("verify-dap error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host runs nx.process + nx.socket on a real nxvim --daemon over WebTransport"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
