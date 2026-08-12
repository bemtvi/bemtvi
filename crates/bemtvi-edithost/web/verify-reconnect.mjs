// Playwright verifier for the browser edit-host's daemon AUTO-RECONNECT (the daemon-reconnect
// plan's Phase 7 — the web/wasm mirror of the native supervisor). The editor runs LOCAL (wasm);
// only the fs/proc/lsp/term seams cross WebTransport, so a dropped link must NOT lose the local
// buffers — the Worker's reconnect supervisor re-dials underneath the seams and re-syncs them.
//
// Flow: spawn a real `bemtvi --daemon --listen`, open the page with `?daemon=<uri>`, open a file
// over the wire, then force the link to drop (window.__bemtvi.debugDropDaemon — a sleep/network
// blip stand-in) and assert: the local buffer survives, `btv.daemon.status()` goes
// `connected → reconnecting → connected`, and — proving the resync re-armed the watch on the
// *new* connection — an external change made AFTER the reconnect autoreloads into the buffer.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi), and a Chromium for Playwright. Run:  node verify-reconnect.mjs
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
async function until(page, fn, pred, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// The current `btv.daemon.status()` phase the page reports. `execLua` renders the result in
// debug form (`…Ok("connected")…`), so extract the bare phase — and note "disconnected" *contains*
// "connected" as a substring, so a naive `.includes` would misclassify it.
async function status(page) {
  const r = await page.evaluate(() =>
    window.__bemtvi.execLua("return btv.daemon.status()").then((r) => String(r.result)),
  );
  const m = r.match(/"(connected|reconnecting|disconnected)"/);
  return m ? m[1] : r;
}

// Poll `btv.daemon.status()` until it reads `want` (or timeout), returning the last phase.
async function untilStatus(page, want, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const s = await status(page);
    if (s === want) return s;
    if (Date.now() - start > ms) return s;
    await sleep(40);
  }
}

// ── The daemon's project file (on real disk; the daemon serves it via StdHostFs) ──────────
const root = mkdtempSync(join(tmpdir(), "bemtvi-reconnect-"));
const file = join(root, "note.txt");
writeFileSync(file, "one"); // no trailing \n → matches lines() exactly

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

  const pageUrl = `http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`;
  await page.goto(pageUrl);

  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // ── 1. Initial connect: open the file over the wire and report `connected` ─────────────
  await page.evaluate((f) => window.__bemtvi.feed(":e " + f), file);
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  const opened = await until(page, () => window.__bemtvi.lines(), (v) => v === "one");
  check("read: :e <file> fills the buffer over the initial WebTransport link", opened === "one", `lines=${JSON.stringify(opened)}`);
  check("status: a fresh daemon session reports connected", (await status(page)) === "connected", `status=${await status(page)}`);

  // ── 2. Force the link to drop — the supervisor must auto-reconnect to the SAME daemon ──
  await page.evaluate(() => window.__bemtvi.debugDropDaemon());
  // The local buffer survives the outage (the editor is local; only the wire dropped).
  const reconnecting = await untilStatus(page, "reconnecting");
  check("status: the dropped link parks reconnecting while it re-dials", reconnecting === "reconnecting", `status=${reconnecting}`);
  const survived = await page.evaluate(() => window.__bemtvi.lines());
  check("survival: the local buffer is intact across the dropped link", survived === "one", `lines=${JSON.stringify(survived)}`);

  // ── 3. Re-stat proof (the closed parity gap): change the file *during* the outage — before
  //       the re-dial re-arms the watch. The re-arm carries the editor's pre-outage baseline,
  //       so the fresh daemon compares it to the changed file and pushes the change; the
  //       unmodified buffer autoreloads. Without the baseline, the daemon would silently
  //       re-baseline the changed file and the buffer would stay "one". ────────────────────
  writeFileSync(file, "changed during the outage");
  const reconnected = await untilStatus(page, "connected");
  check("status: the link auto-recovers to connected with no manual action", reconnected === "connected", `status=${reconnected}`);
  const reloaded = await until(page, () => window.__bemtvi.lines(), (v) => v === "changed during the outage");
  check(
    "re-stat: a change made DURING the outage autoreloads after reconnect (baseline threaded to re-arm)",
    reloaded === "changed during the outage",
    `lines=${JSON.stringify(reloaded)}`,
  );

  // ── 4. The wire genuinely re-points: a `:w` after reconnect lands on the daemon's disk ──
  await page.evaluate(() => window.__bemtvi.feed("Gosaved after reconnect<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed(":w"));
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  let onDisk = "";
  for (let i = 0; i < 80; i++) {
    onDisk = (await import("node:fs")).readFileSync(file, "utf8");
    if (onDisk.includes("saved after reconnect")) break;
    await sleep(40);
  }
  check("write: a :w after the reconnect crosses the new wire to the daemon's disk", onDisk.includes("saved after reconnect"), `disk=${JSON.stringify(onDisk)}`);

  await browser.close();
} catch (e) {
  console.error("verify-reconnect error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — the browser edit-host auto-reconnects a dropped daemon link and re-syncs its seams"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
