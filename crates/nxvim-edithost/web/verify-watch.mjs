// Playwright verifier for the browser edit-host's **watch leg** over WebTransport (Phase 6 —
// the daemon `HostWatch` push direction). Companion to verify-daemon.mjs (the fs read/write
// leg): there the Worker always *initiates* the round-trip; here the daemon **owns change
// detection** and *pushes* `fs_changed` when a watched file drifts, which the browser turns
// into a `FileChangedShell` reconcile (autoreload / handler choice) off the editor tick. This
// is the browser twin of the native daemon_watch.rs test — only the transport (real
// WebTransport/QUIC to a real `nxvim --daemon --listen`) and the change source (Node rewriting
// the file on the daemon's disk) differ.
//
// Two asserts, both proving the push truly crossed the wire (the file lives on the daemon's
// disk, which the browser origin can't touch, and there is NO `:checktime` — the daemon's
// watch drove it on its own):
//   1. an external change to an *unmodified* buffer **autoreloads** the new bytes, and
//   2. a `FileChangedShell` handler fires on the edit-host with `v:fcs_reason` set and its
//      `v:fcs_choice = "reload"` drives the off-tick re-fetch.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-watch.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8141;
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
async function until(page, fn, pred, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// ── The daemon's project tree (on real disk; the daemon serves it via StdHostFs) ──────────
const root = mkdtempSync(join(tmpdir(), "nxvim-watch-"));
const noteFile = join(root, "note.txt"); // for the autoreload assert
const docFile = join(root, "doc.txt"); //  for the FileChangedShell assert
writeFileSync(noteFile, "alpha"); // no trailing \n → matches lines() exactly
writeFileSync(docFile, "first");

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

  // ── 1. Autoreload: open the file (arms a watch), then change it on the daemon's disk ──────
  await page.evaluate((f) => window.__nxvim.feed(":e " + f), noteFile);
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  const opened = await until(page, () => window.__nxvim.lines(), (v) => v === "alpha");
  check("setup: :e <file> opens the daemon's file (and arms the watch)", opened === "alpha", `lines=${JSON.stringify(opened)}`);

  // Someone else rewrites the remote file (Node, on the daemon's disk — the browser origin
  // can't touch it). NO :checktime: the daemon's watch must detect + push it on its own.
  writeFileSync(noteFile, "alpha\nbeta\ngamma");
  const reloaded = await until(page, () => window.__nxvim.lines(), (v) => v === "alpha\nbeta\ngamma");
  check(
    "autoreload: an external change to an unmodified buffer reloads over the WebTransport watch push",
    reloaded === "alpha\nbeta\ngamma",
    `lines=${JSON.stringify(reloaded)} want=${JSON.stringify("alpha\nbeta\ngamma")}`,
  );

  // ── 2. FileChangedShell: 'noautoread' + a handler that records the reason and reloads ─────
  await page.evaluate((f) => window.__nxvim.feed(":e " + f), docFile);
  await page.evaluate(() => window.__nxvim.feed("<CR>"));
  await until(page, () => window.__nxvim.lines(), (v) => v === "first");

  await page.evaluate(() =>
    window.__nxvim.execLua(`
      vim.o.autoread = false
      vim.g.fcs_reason = ""
      vim.api.nvim_create_autocmd("FileChangedShell", {
        callback = function()
          vim.g.fcs_reason = vim.v.fcs_reason
          vim.v.fcs_choice = "reload"
        end,
      })
      return 1
    `),
  );

  writeFileSync(docFile, "second\nthird");
  const handled = await until(page, () => window.__nxvim.lines(), (v) => v === "second\nthird");
  check(
    "FileChangedShell: the handler's v:fcs_choice='reload' re-fetches the new bytes over the wire",
    handled === "second\nthird",
    `lines=${JSON.stringify(handled)} want=${JSON.stringify("second\nthird")}`,
  );

  const reason = await page.evaluate(() =>
    window.__nxvim.execLua("return vim.g.fcs_reason").then((r) => r.result),
  );
  check(
    "FileChangedShell: the handler saw v:fcs_reason over the wire (unmodified, present ⇒ 'changed')",
    /changed/.test(String(reason)),
    `fcs_reason=${JSON.stringify(reason)}`,
  );

  await browser.close();
} catch (e) {
  console.error("verify-watch error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host reconciles daemon-pushed file changes over the WebTransport watch leg"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
