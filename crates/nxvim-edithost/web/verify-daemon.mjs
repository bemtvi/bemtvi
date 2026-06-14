// Playwright verifier for the browser edit-host driving a REAL `nxvim --daemon --listen`
// over WebTransport (Phase 6b — the daemon fs leg). Unlike verify-fs.mjs (the File System
// Access picker) and the OPFS path in verify.mjs, the files here live on the *daemon's*
// disk and cross a real WebTransport/QUIC connection — so a buffer's bytes can only have
// come over the wire, and a `:w` is proven by reading the file back from the daemon's disk
// in Node. This is the browser twin of the native daemon_quic.rs test.
//
// Flow: spawn the daemon (an ephemeral loopback listener), parse its launch-printed connect
// URI, open the page with `?daemon=<uri>` so the Worker dials it, then drive `:e`/`:w`/`:e
// <dir>` through window.__nxvim and assert the bytes round-trip end to end.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-daemon.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync, mkdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8140;
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
async function until(page, fn, pred, ms = 6000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// ── The daemon's project tree (on real disk; the daemon serves it via StdHostFs) ──────────
const root = mkdtempSync(join(tmpdir(), "nxvim-daemon-"));
const file = join(root, "hello.txt");
const FILE_CONTENT = "alpha\nbeta\ngamma"; // no trailing \n → matches lines() exactly
writeFileSync(file, FILE_CONTENT);
const sub = join(root, "sub");
mkdirSync(sub);
writeFileSync(join(sub, "one.txt"), "1");
writeFileSync(join(sub, "two.txt"), "2");

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

  // ── 1. `:e <file>` reads the daemon's file over the wire ──────────────────────────────
  await page.evaluate((f) => window.__nxvim.feed(":e " + f), file);
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  const opened = await until(page, () => window.__nxvim.lines(), (v) => v === FILE_CONTENT);
  check(
    "read: :e <file> fills the buffer with the daemon's bytes (over WebTransport)",
    opened === FILE_CONTENT,
    `lines=${JSON.stringify(opened)} want=${JSON.stringify(FILE_CONTENT)}`,
  );

  // The buffer is named for the remote path (a daemon-fetched replica, not the local disk).
  const named = await page.evaluate(
    () => (window.__nxvim.frame()?.windows?.find((w) => w.focused) || {}).file_name || "",
  );
  check("read: the buffer is bound to the remote path", String(named).includes("hello.txt"), `name=${JSON.stringify(named)}`);

  // ── 2. Edit + `:w` — modified clears only on the ack; bytes land on the daemon's disk ──
  await page.evaluate(() => window.__nxvim.feed("GoDELTA-OVER-WIRE<Esc>"));
  // exec_lua renders an integer plainly (`ok:1`), so use the `and 1 or 0` idiom (verify.mjs).
  const dirty = await page.evaluate(() =>
    window.__nxvim.execLua("return vim.bo.modified and 1 or 0").then((r) => r.result),
  );
  check("write: buffer is modified after the edit", /1$/.test(String(dirty)), `modified=${JSON.stringify(dirty)}`);

  await page.evaluate(() => window.__nxvim.feed(":w"));
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  // modified clears only after the daemon acks the write (ack-gated finalize).
  const cleared = await until(
    page,
    () => window.__nxvim.execLua("return vim.bo.modified and 1 or 0").then((r) => r.result),
    (v) => /0$/.test(String(v)),
  );
  check("write: modified clears after the daemon acks :w", /0$/.test(String(cleared)), `modified=${JSON.stringify(cleared)}`);

  // The edited bytes are now on the *daemon's* disk — read them back in Node (a path the
  // browser origin can't touch), proving the write truly crossed the wire.
  let onDisk = "";
  for (let i = 0; i < 50; i++) {
    onDisk = readFileSync(file, "utf8");
    if (onDisk.includes("DELTA-OVER-WIRE")) break;
    await sleep(40);
  }
  check(
    "write: the edited bytes landed on the daemon's disk (read back in Node)",
    onDisk.includes("alpha") && onDisk.includes("DELTA-OVER-WIRE"),
    `disk=${JSON.stringify(onDisk)}`,
  );

  // ── 3. `:e <dir>` lists the remote directory over the wire ────────────────────────────
  await page.evaluate((d) => window.__nxvim.feed(":e " + d), sub);
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  const listing = await until(
    page,
    () => window.__nxvim.lines(),
    (v) => v.includes("one.txt") && v.includes("two.txt"),
  );
  check(
    "explorer: :e <dir> lists the daemon's directory entries (over WebTransport)",
    listing.includes("one.txt") && listing.includes("two.txt"),
    `listing=${JSON.stringify(listing)}`,
  );

  await browser.close();
} catch (e) {
  console.error("verify-daemon error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host opens/saves/lists files on a real nxvim --daemon over WebTransport"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
