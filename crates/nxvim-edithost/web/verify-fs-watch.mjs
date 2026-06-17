// Playwright verifier for the browser edit-host's streaming `nx.fs.watch` over a REAL
// `nxvim --daemon --listen` (Phase 3b of the off-tick plan). A browser `nx.fs.watch` has no local
// filesystem watcher — the arm crosses the wire to the daemon's recursive `notify` watcher (the
// same coalescing watcher the native `nx.fs.watch` rides), and each coalesced change batch returns
// as a `luafs_change` push the Worker lands into the watch stream's `:next()`. The browser twin of
// the native nx.fs watch leg; daemon-only (serverless OPFS has no change source).
//
// Faithfulness (not a no-op): (1) a file CREATED on the daemon's disk by Node (a path the browser
// origin can't touch) surfaces in the browser's watch stream with its path + a change kind; (2)
// arming a watch on a nonexistent path REJECTS the stream loud (the error envelope round-trips);
// (3) `:stop()` ends the iteration (a parked `:next()` resolves nil).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`, Chromium.
// Run:  node verify-fs-watch.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8144;
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

const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);
// Poll a `_G.__<name>` global (rendered through rmpv Debug, so unset reads as `…Ok("nil")…`).
async function pollGlobal(page, g, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__nxvim.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

const root = mkdtempSync(join(tmpdir(), "nxvim-fswatch-"));
const watchDir = join(root, "tree");
mkdirSync(watchDir); // notify needs the watched path to exist before arming
const newFile = join(watchDir, "appeared.txt");

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

  await page.goto(`http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`);
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted + dialed the daemon", true);

  // ── 1. a change on the DAEMON's disk surfaces in the browser watch stream ──────────────────
  // Arm a recursive watch; its first change batch lands in `_G.__ev`. The watch is kept alive
  // in `_G.__watch` so it isn't GC'd before the change arrives.
  await luaResult(page, `_G.__ev = nil
     _G.__watch = nx.fs.watch("${watchDir}", { recursive = true })
     _G.__watch:next():next(
       function(ev) if ev then _G.__ev = ev.kind .. ":[" .. table.concat(ev.paths, ",") .. "]" end end,
       function(e) _G.__ev = "ERR:" .. e.message end)
     return 1`);
  // Give the arm time to cross the wire and the daemon's notify watcher to establish.
  await sleep(700);
  writeFileSync(newFile, "I appeared on the daemon disk");
  const ev = await pollGlobal(page, "__ev");
  check("nx.fs.watch surfaces a daemon-side change (kind + path) over WebTransport",
    /appeared\.txt/.test(String(ev)) && /(create|modify|rename)/.test(String(ev)),
    `ev=${JSON.stringify(ev)}`);

  // ── 2. arming a watch on a nonexistent path REJECTS the stream loud ────────────────────────
  await luaResult(page, `_G.__werr = nil
     _G.__badwatch = nx.fs.watch("${join(root, "does-not-exist")}", {})
     _G.__badwatch:next():next(
       function(_) _G.__werr = "RESOLVED?!" end,
       function(e) _G.__werr = "REJECTED" end)
     return 1`);
  const werr = await pollGlobal(page, "__werr");
  check("nx.fs.watch on a missing path rejects the stream (error envelope round-trips)",
    /REJECTED/.test(String(werr)), `werr=${JSON.stringify(werr)}`);

  // ── 3. :stop() ends the iteration (a fresh :next() resolves nil) ───────────────────────────
  await luaResult(page, `_G.__stopped = nil
     local w = nx.fs.watch("${watchDir}", { recursive = true })
     w:stop()
     w:next():next(function(ev) _G.__stopped = (ev == nil) and "nil-end" or "GOT-EVENT" end,
       function(e) _G.__stopped = "ERR:"..e.message end)
     return 1`);
  const stopped = await pollGlobal(page, "__stopped");
  check("nx.fs.watch :stop() ends the iteration (next() resolves nil)",
    /nil-end/.test(String(stopped)), `stopped=${JSON.stringify(stopped)}`);

  await browser.close();
} catch (e) {
  console.error("verify-fs-watch error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser nx.fs.watch streams daemon-side changes over WebTransport (change, error, stop)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
