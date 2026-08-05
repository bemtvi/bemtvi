// Playwright verifier for the browser edit-host's PROC leg (Phase 6d) against a REAL
// `nxvim --daemon --listen` over WebTransport. An async `vim.system` in the browser has no
// local process to run — the spawn crosses the wire to the daemon, runs there, and its
// stdout/exit return as `proc_spawned`/`proc_exited` pushes the Worker lands into the tick's
// `on_exit` callback. The browser twin of the native daemon_proc.rs test.
//
// Faithfulness (not a no-op): (1) the command's stdout round-trips back to the callback; (2)
// the command writes a marker file on the *daemon's* disk — a path the browser origin can't
// touch — which Node reads back, proving the process truly executed on the daemon; (3) a
// `sleep 30` child is killed from the browser and its `on_exit` fires with a -1 (killed) code
// in well under a second, proving `proc_kill` crosses the wire and terminates the child.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-proc.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8141;
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
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// ── The daemon's working tree (real disk; the marker file lands here) ──────────────────────
const root = mkdtempSync(join(tmpdir(), "nxvim-proc-"));
const marker = join(root, "proc-marker.txt");

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

  // The async spawn is driven through the real `nx._system_async` funnel (the same one the
  // public `nx.run` / `vim.system` wrappers call — `nx.run` builds the argv and registers a
  // callback id, then hands off to this funnel; the native proc leg is funnel-only too).
  // A callback id is registered in `nx._cb_fns`, exactly as the wrapper would, so this
  // exercises the genuine spawn → wire → exit → on_exit path, not a mock.

  // ── 1. async spawn: stdout round-trips back to the on_exit callback over the wire ──────
  await luaResult(
    page,
    `_G.__out, _G.__code = nil, nil
     local id = nx._next_cb_id()
     nx._cb_fns[id] = function(res) _G.__out, _G.__code = res.stdout, res.code end
     nx._system_async(id, { "sh", "-c", "printf hello-from-daemon" }, nil, nil, nil)
     return 1`,
  );
  const out = await until(page, () => window.__nxvim.execLua("return _G.__out or ''").then((r) => r.result),
    (v) => /hello-from-daemon/.test(String(v)));
  check("proc: vim.system on_exit sees the daemon child's stdout (over WebTransport)",
    /hello-from-daemon/.test(String(out)), `out=${JSON.stringify(out)}`);

  const code0 = await luaResult(page, "return (_G.__code == 0) and 1 or 0");
  check("proc: the child's exit code (0) round-trips to on_exit", /1$/.test(String(code0)), `code=${JSON.stringify(code0)}`);

  // ── 2. The process truly ran ON THE DAEMON: it wrote a marker file Node reads back ──────
  await luaResult(
    page,
    `local id = nx._next_cb_id()
     nx._cb_fns[id] = function() _G.__wrote = true end
     nx._system_async(id, { "sh", "-c", "printf RAN-ON-DAEMON > ${marker}" }, nil, nil, nil)
     return 1`,
  );
  await until(page, () => window.__nxvim.execLua("return _G.__wrote and 1 or 0").then((r) => r.result),
    (v) => /1$/.test(String(v)));
  let onDisk = "";
  for (let i = 0; i < 50; i++) {
    if (existsSync(marker)) { onDisk = readFileSync(marker, "utf8"); if (onDisk.includes("RAN-ON-DAEMON")) break; }
    await sleep(40);
  }
  check("proc: the child executed on the daemon (marker file written to the daemon's disk)",
    onDisk.includes("RAN-ON-DAEMON"), `marker=${JSON.stringify(onDisk)}`);

  // ── 3. a kill terminates a daemon child over the wire; on_exit fires (code -1) ──────────
  await luaResult(
    page,
    `_G.__killed_code = nil
     _G.__kid = nx._next_cb_id()
     nx._cb_fns[_G.__kid] = function(res) _G.__killed_code = res.code end
     nx._system_async(_G.__kid, { "sh", "-c", "sleep 30" }, nil, nil, nil)
     return 1`,
  );
  // Give the spawn a beat to register on the daemon, then kill it.
  await sleep(200);
  await luaResult(page, "nx._system_kill(_G.__kid, nil) return 1");
  const killedCode = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__killed_code)").then((r) => r.result),
    (v) => !/nil/.test(String(v)), 5000); // the sleep is 30s — a fire well under that proves the kill landed
  check("proc: handle:kill() fires on_exit (kill crossed the wire, didn't wait out sleep 30)",
    !/nil/.test(String(killedCode)), `killed_code=${JSON.stringify(killedCode)}`);
  check("proc: a killed child reports a -1 exit code", /-1/.test(String(killedCode)), `killed_code=${JSON.stringify(killedCode)}`);

  await browser.close();
} catch (e) {
  console.error("verify-proc error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host runs vim.system on a real nxvim --daemon over WebTransport (stdout, daemon-side effect, kill)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
