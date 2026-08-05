// Playwright verifier for the browser edit-host's `nx.fs` leg (Phase 2 of the off-tick plan)
// against a REAL `nxvim --daemon --listen` over WebTransport. A browser `nx.fs.*` op has no
// local synchronous filesystem to run — the op crosses the wire to the daemon as one `luafs_op`
// request, runs there through `run_fs_job`, and its typed result returns and resolves the op's
// promise in the tick. The browser twin of the native off-tick `nx.fs` actor leg.
//
// Faithfulness (not a no-op, and NOT the in-browser MEMFS — the whole point):
//   (1) read_text returns the content of a file that exists ONLY on the daemon's disk (Node
//       wrote it to the daemon's temp tree — a path the browser origin can't otherwise touch);
//   (2) readdir lists entries that exist only on the daemon's disk;
//   (3) write creates a file ON THE DAEMON — Node reads it back from the daemon's tree, proving
//       the op truly mutated the remote fs, not a browser-local shadow;
//   (4) a missing path REJECTS with err.code == "ENOENT" (the error envelope round-trips).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-fs-op.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, readFileSync, writeFileSync, mkdirSync, existsSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8142;
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

// `execLua().result` renders as `ok:<rmpv Debug>` / `err:<msg>` (see index.html execLuaOp); a
// string value shows as `String(Utf8String { s: Ok("<content>") })`. Extract <content> for an
// exact compare (substring/regex checks tolerate the wrapper, but a path equality needs it clean).
function plainStr(v) {
  const m = String(v).match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1].replace(/\\n/g, "\n").replace(/\\"/g, '"').replace(/\\\\/g, "\\") : String(v);
}

// ── The daemon's working tree (real disk; the browser reads/writes it OVER THE WIRE) ─────────
const root = mkdtempSync(join(tmpdir(), "nxvim-fsop-"));
const readFile = join(root, "hello.txt");
writeFileSync(readFile, "HELLO-FROM-DAEMON-DISK\nsecond line\n");
const listDir = join(root, "listing");
mkdirSync(listDir);
writeFileSync(join(listDir, "alpha.txt"), "a");
writeFileSync(join(listDir, "beta.txt"), "b");
mkdirSync(join(listDir, "subdir"));
const writeTarget = join(root, "written-by-browser.txt");
const missing = join(root, "does-not-exist.txt");
// A subdir under the daemon's cwd, holding a file addressed by a RELATIVE path after `:cd`.
const deepDir = join(root, "deep");
mkdirSync(deepDir);
writeFileSync(join(deepDir, "nested.txt"), "NESTED-VIA-RELATIVE-PATH\n");

// ── Spawn the real daemon (cwd = `root`, so the session's cwd seeds there) ──────────────────
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

  // ── 1. read_text: a file that exists ONLY on the daemon's disk round-trips over the wire ──
  await luaResult(page, `_G.__text, _G.__terr = nil, nil
     nx.fs.read_text("${readFile}"):next(
       function(t) _G.__text = t end,
       function(e) _G.__terr = e.message end)
     return 1`);
  const text = await until(page,
    () => window.__nxvim.execLua("return _G.__text or ''").then((r) => r.result),
    (v) => /HELLO-FROM-DAEMON-DISK/.test(String(v)));
  check("nx.fs.read_text returns the daemon file's content (over WebTransport, not MEMFS)",
    /HELLO-FROM-DAEMON-DISK/.test(String(text)) && /second line/.test(String(text)),
    `text=${JSON.stringify(text)}`);

  // ── 2. readdir: entries that exist only on the daemon's disk, with their dirent kinds ──────
  await luaResult(page, `_G.__names, _G.__rderr = nil, nil
     nx.fs.readdir("${listDir}"):next(function(entries)
       local out = {}
       for _, e in ipairs(entries) do out[#out+1] = e.name .. ":" .. e.type end
       table.sort(out)
       _G.__names = table.concat(out, ",")
     end, function(e) _G.__rderr = e.message end)
     return 1`);
  const names = await until(page,
    () => window.__nxvim.execLua("return _G.__names or ''").then((r) => r.result),
    (v) => /alpha\.txt/.test(String(v)));
  check("nx.fs.readdir lists the daemon dir's entries with kinds (file/directory)",
    /alpha\.txt:file/.test(String(names)) &&
    /beta\.txt:file/.test(String(names)) &&
    /subdir:directory/.test(String(names)),
    `names=${JSON.stringify(names)}`);

  // ── 3. write: the op truly mutates the DAEMON's disk — Node reads the file back ────────────
  await luaResult(page, `_G.__wrote, _G.__werr = nil, nil
     nx.fs.write("${writeTarget}", "WROTE-FROM-BROWSER"):next(
       function() _G.__wrote = true end,
       function(e) _G.__werr = e.message end)
     return 1`);
  await until(page,
    () => window.__nxvim.execLua("return _G.__wrote and 1 or (_G.__werr or 0)").then((r) => r.result),
    (v) => /1$/.test(String(v)) || /[A-Za-z]/.test(String(v)));
  let onDisk = "";
  for (let i = 0; i < 50; i++) {
    if (existsSync(writeTarget)) { onDisk = readFileSync(writeTarget, "utf8"); if (onDisk.includes("WROTE-FROM-BROWSER")) break; }
    await sleep(40);
  }
  check("nx.fs.write created the file ON THE DAEMON (Node reads it back from the daemon's tree)",
    onDisk.includes("WROTE-FROM-BROWSER"), `onDisk=${JSON.stringify(onDisk)}`);

  // ── 4. a missing path REJECTS with err.code == "ENOENT" (the error envelope round-trips) ───
  await luaResult(page, `_G.__code = nil
     nx.fs.read_text("${missing}"):next(
       function(_) _G.__code = "RESOLVED?!" end,
       function(e) _G.__code = e.code end)
     return 1`);
  const code = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__code)").then((r) => r.result),
    (v) => !/^nil$/.test(String(v)));
  check("nx.fs read of a missing daemon path rejects with err.code == ENOENT",
    /ENOENT/.test(String(code)), `code=${JSON.stringify(code)}`);

  // ── 5. a LOCAL fs op (the plugin manager's seam) hits OPFS, NOT the daemon ─────────────────
  // Plugin management is local even in a daemon session (plugins load into the local VM). The
  // low-level `nx._local_fs_op` seam the manager uses routes to the local OPFS store, so a file
  // that exists ONLY on the daemon's disk is INVISIBLE to it — while a session `nx.fs.exists`
  // sees it over the wire. The two disagreeing on the SAME path is the proof of the split.
  await luaResult(page, `_G.__sess, _G.__loc, _G.__locerr = nil, nil, nil
     nx.fs.exists("${readFile}"):next(function(v) _G.__sess = v end)
     do
       local id = nx._next_cb_id()
       nx._cb_fns[id] = function(err, value)
         if err ~= nil then _G.__locerr = tostring(err.message or err) else _G.__loc = value end
       end
       nx._local_fs_op({ op = "exists", path = "${readFile}" }, id)
     end
     return 1`);
  const split = plainStr(await until(page,
    () => window.__nxvim.execLua(
      "return tostring(_G.__sess) .. '/' .. tostring(_G.__loc) .. '/' .. tostring(_G.__locerr)"
    ).then((r) => r.result),
    (v) => /(true|false)\/(true|false)/.test(plainStr(v))));
  check("a LOCAL fs op routes to OPFS, not the daemon (session sees the daemon file; local does not)",
    /^true\/false/.test(split), `sess/loc/err=${JSON.stringify(split)}`);

  // ── 6. RELATIVE nx.fs path resolves against the session cwd and FOLLOWS a remote `:cd` ──────
  // The session cwd seeds from the daemon's cwd (`root`); a relative `nx.fs` path must be
  // absolutized against it (the edit-host's `DirState`) before crossing the wire, because the
  // daemon is stateless and would otherwise resolve `.` against its own launch dir. Without the
  // rebase (the bug), `nx.fs.read_text("nested.txt")` after `:cd deep` ENOENTs / hits the wrong dir.
  const rootReal = realpathSync(root);
  const cwd0 = plainStr(await luaResult(page, "return vim.fn.getcwd()"));
  check("web daemon session seeds its cwd from the daemon (getcwd == daemon cwd)",
    cwd0 === rootReal, `getcwd=${JSON.stringify(cwd0)} want=${JSON.stringify(rootReal)}`);

  // `:cd deep` (relative) moves the session cwd into <root>/deep, then a relative read.
  await luaResult(page, 'vim.cmd("cd deep") return 1');
  const cwd1 = plainStr(await until(page,
    () => window.__nxvim.execLua("return vim.fn.getcwd()").then((r) => r.result),
    (v) => /\/deep"\)/.test(String(v))));
  check("a relative `:cd deep` moves the web session cwd into the subdirectory",
    cwd1 === join(rootReal, "deep"), `getcwd=${JSON.stringify(cwd1)}`);

  await luaResult(page, `_G.__rel, _G.__relerr = nil, nil
     nx.fs.read_text("nested.txt"):next(
       function(t) _G.__rel = t end,
       function(e) _G.__relerr = e.code .. ":" .. e.message end)
     return 1`);
  const rel = await until(page,
    () => window.__nxvim.execLua("return _G.__rel or _G.__relerr or ''").then((r) => r.result),
    (v) => /NESTED-VIA-RELATIVE-PATH|ENOENT/.test(String(v)));
  check("a RELATIVE nx.fs.read_text after `:cd` resolves against the new cwd (over the wire)",
    /NESTED-VIA-RELATIVE-PATH/.test(String(rel)), `rel=${JSON.stringify(rel)}`);

  await browser.close();
} catch (e) {
  console.error("verify-fs-op error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser nx.fs runs on a real nxvim --daemon over WebTransport (read_text, readdir, write-to-daemon, ENOENT, local→OPFS split)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
