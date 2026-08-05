// Playwright verifier for the browser edit-host's RUNTIME `:connect` cwd seed. A `?daemon=` boot
// seeds the session cwd from the daemon's `config_bundle`; a *runtime* `:connect nxvim://…` re-points
// the fs seam WITHOUT re-fetching the bundle, so the daemon's cwd used to never reach `DirState` and
// a relative `nx.fs` path stayed unrebased (resolving against the stale serverless dir). This proves
// the fix: the Worker fetches the new daemon's cwd (`realpath(".")`) on runtime connect and seeds it,
// so `getcwd()` reports the daemon cwd and a RELATIVE `nx.fs.read_text` resolves against it over the wire.
//
// Boots SERVERLESS (no `?daemon=`), asserts the relative read does NOT reach the daemon file (OPFS,
// ENOENT), then `:connect`s at runtime and asserts it now does — the before/after that isolates the seed.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-connect-cwd.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8144;
const NXVIM = process.env.NXVIM_BIN || `${here}../../../target/debug/nxvim`;

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

// `execLua().result` renders as `ok:<rmpv Debug>`; a string shows as `String(Utf8String { s: Ok("<c>") })`.
function plainStr(v) {
  const m = String(v).match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1].replace(/\\n/g, "\n").replace(/\\"/g, '"').replace(/\\\\/g, "\\") : String(v);
}
// The current `nx.daemon.status()` phase ("disconnected" contains "connected", so match the quotes).
async function status(page) {
  const r = plainStr(await luaResult(page, "return nx.daemon.status()"));
  const m = String(r).match(/(connected|reconnecting|disconnected)/);
  return m ? m[1] : String(r);
}

// ── The daemon's working tree (real disk; the daemon's cwd = `root`) ─────────────────────────
const root = mkdtempSync(join(tmpdir(), "nxvim-connect-cwd-"));
writeFileSync(join(root, "marker.txt"), "MARKER-ON-DAEMON-CWD\n");
const rootReal = realpathSync(root);

// ── Spawn the real daemon (cwd = `root`) ─────────────────────────────────────────────────────
const daemon = spawn(NXVIM, ["--daemon", "--listen", "127.0.0.1:0"], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
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

// Run a relative `nx.fs.read_text("marker.txt")` and return its content, or `ERR:<code>` on reject.
async function relRead(page) {
  await luaResult(page, `_G.__r, _G.__e = nil, nil
     nx.fs.read_text("marker.txt"):next(
       function(t) _G.__r = t end,
       function(e) _G.__e = "ERR:" .. e.code end)
     return 1`);
  return plainStr(await until(page,
    () => window.__nxvim.execLua("return _G.__r or _G.__e or ''").then((r) => r.result),
    (v) => /MARKER-ON-DAEMON-CWD|ERR:/.test(String(v))));
}

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
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // SERVERLESS boot — NO `?daemon=`. The session's cwd is the serverless default, not the daemon's.
  await page.goto(`http://localhost:${PORT}/web/`);
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Before connect: serverless, so a relative read hits OPFS (no marker there) → ENOENT, NOT the daemon.
  const before = await relRead(page);
  check("before `:connect`: a relative nx.fs read hits OPFS (not the daemon) — ENOENT",
    /ERR:ENOENT/.test(before), `before=${JSON.stringify(before)}`);
  const cwdBefore = plainStr(await luaResult(page, "return vim.fn.getcwd()"));
  check("before `:connect`: getcwd is NOT the daemon cwd (serverless default)",
    cwdBefore !== rootReal, `getcwd=${JSON.stringify(cwdBefore)}`);

  // Runtime `:connect nxvim://…` — typed on the command line, confirmed through the UI intercept
  // (`pressEnter` routes via tryConnectIntercept, exactly as a real <CR> would).
  await page.evaluate((u) => window.__nxvim.feed(":connect " + u), uri);
  await page.evaluate(() => window.__nxvim.pressEnter());
  const phase = await until(page, () => window.__nxvim.execLua("return nx.daemon.status()").then((r) => r.result),
    (v) => /connected/.test(String(v)) && !/disconnected/.test(String(v)));
  check("runtime `:connect` dials the daemon (status → connected)",
    /(^|[^s])connected/.test(String(phase)) && (await status(page)) === "connected", `status=${await status(page)}`);

  // After connect: getcwd seeds from the daemon's cwd (the fix — a runtime connect now seeds DirState).
  const cwdAfter = plainStr(await until(page,
    () => window.__nxvim.execLua("return vim.fn.getcwd()").then((r) => r.result),
    (v) => new RegExp(rootReal.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).test(String(v))));
  check("runtime `:connect` seeds the session cwd from the new daemon (getcwd == daemon cwd)",
    cwdAfter === rootReal, `getcwd=${JSON.stringify(cwdAfter)} want=${JSON.stringify(rootReal)}`);

  // After connect: the SAME relative read now resolves against the daemon cwd and reads over the wire.
  const after = await relRead(page);
  check("after `:connect`: a RELATIVE nx.fs read resolves against the daemon cwd (over the wire)",
    /MARKER-ON-DAEMON-CWD/.test(after), `after=${JSON.stringify(after)}`);

  await browser.close();
} catch (e) {
  console.error("verify-connect-cwd error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — a runtime :connect seeds the session cwd from the daemon; a relative nx.fs path then follows it"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
