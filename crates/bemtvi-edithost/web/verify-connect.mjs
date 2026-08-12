// Playwright verifier for the runtime `:connect bemtvi://…` command (the browser twin of
// bemtvi-gui's client-side `:connect`). Unlike verify-daemon.mjs — which hands the daemon URI
// to the page up front via `?daemon=` so the Worker dials it at boot — here the page opens
// **serverless** (OPFS, no param) and the daemon link is brought up *at runtime* by typing
// `:connect bemtvi://…` and pressing Enter through the real keydown interception. The proof is
// that a `:e <daemon file>` issued *after* the connect fills the buffer with the daemon's bytes
// (which can only have crossed the WebTransport wire), and a bad/rejected URI is surfaced loudly
// without touching the wire.
//
// Flow: spawn the daemon (ephemeral loopback listener), parse its launch-printed URI, open the
// page WITHOUT `?daemon=`, then drive `:connect <uri>` + pressEnter via window.__bemtvi and assert
// the off-tick fs seam re-points onto the wire.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi), and a Chromium for Playwright. Run:  node verify-connect.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8141;
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
const root = mkdtempSync(join(tmpdir(), "bemtvi-connect-"));
const file = join(root, "remote.txt");
const FILE_CONTENT = "one\ntwo\nthree"; // no trailing \n → matches lines() exactly
writeFileSync(file, FILE_CONTENT);

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

  // Open SERVERLESS — no `?daemon=`. The link is brought up at runtime by `:connect` below.
  await page.goto(`http://localhost:${PORT}/web/`);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted serverless (no ?daemon=); window.__bemtvi.ready resolved", true);

  // ── 1. A non-bemtvi:// argument is rejected loudly, the wire untouched ──────────────────
  await page.evaluate(() => window.__bemtvi.feed(":connect nvim://oops"));
  const rejected = await page.evaluate(() => window.__bemtvi.pressEnter());
  check(
    "reject: :connect with a non-bemtvi:// URI is intercepted (handled client-side)",
    rejected && rejected.intercepted === "connect",
    `pressEnter=${JSON.stringify(rejected)}`,
  );
  const statusAfterReject = await until(
    page,
    () => document.getElementById("status")?.textContent || "",
    (v) => /bemtvi:\/\//.test(v),
    2000,
  );
  check(
    "reject: the corner indicator flags the bad URI",
    /bemtvi:\/\//.test(statusAfterReject),
    `status=${JSON.stringify(statusAfterReject)}`,
  );

  // ── 2. `:connect bemtvi://…` dials the real daemon at runtime ───────────────────────────
  await page.evaluate((u) => window.__bemtvi.feed(":connect " + u), uri);
  const connected = await page.evaluate(() => window.__bemtvi.pressEnter());
  check(
    "connect: :connect bemtvi://… is intercepted (the editor core never sees it)",
    connected && connected.intercepted === "connect",
    `pressEnter=${JSON.stringify(connected)}`,
  );

  // ── 3. A `:e <daemon file>` issued AFTER the connect reads over the wire ───────────────
  // Bytes appearing here prove the off-tick fs seam re-pointed from OPFS onto the daemon: the
  // file lives only on the daemon's disk, so an OPFS read would return a NEW empty buffer.
  await page.evaluate((f) => window.__bemtvi.feed(":e " + f), file);
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  const opened = await until(page, () => window.__bemtvi.lines(), (v) => v === FILE_CONTENT, 8000);
  check(
    "connect: :e <daemon file> after :connect reads the daemon's bytes over the wire",
    opened === FILE_CONTENT,
    `lines=${JSON.stringify(opened)} want=${JSON.stringify(FILE_CONTENT)}`,
  );

  // The buffer is named for the remote path (a daemon-fetched replica, not a local OPFS file).
  const named = await page.evaluate(
    () => (window.__bemtvi.frame()?.windows?.find((w) => w.focused) || {}).file_name || "",
  );
  check(
    "connect: the buffer is bound to the remote path",
    String(named).includes("remote.txt"),
    `name=${JSON.stringify(named)}`,
  );

  await browser.close();
} catch (e) {
  console.error("verify-connect error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — runtime :connect bemtvi://… brings up the daemon fs seam at runtime (no ?daemon= param)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
