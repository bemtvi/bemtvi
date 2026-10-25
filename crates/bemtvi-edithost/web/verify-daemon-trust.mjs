// Playwright verifier for DAEMON TRUST on the browser edit-host. A page link can carry
// `?daemon=bemtvi://HOST:PORT/TOKEN?cert=HASH`, and a daemon session runs that daemon's
// `init.lua` in the page's Lua VM — so a hostile link must neither connect silently nor see
// the local session's persisted state (registers, history, marks).
//
//   1. An unapproved `?daemon=` link does NOT connect (no Worker, no dial) until the user
//      confirms the in-page dialog, which names HOST:PORT and the cert hash.
//   2. A daemon session can't read the local session's registers: its shada is its own OPFS
//      blob, scoped to the daemon (the local blob is untouched and unread).
//   3. A daemon's own state persists across reloads (its namespace works), and an approved
//      daemon reconnects with no prompt.
//   4. The local session never sees the daemon session's registers.
//   5. Declining keeps the page serverless, with a visible note; a different cert at the same
//      address prompts again.
//
// Prereqs: ./build.sh, `cargo build -p bemtvi` (target/debug/bemtvi), a Chromium for
// Playwright. Run:  node verify-daemon-trust.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8147;
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

// exec_lua's result arrives as its debug rendering (`ok:String(Utf8String { s: Ok("…") })`);
// unwrap a string result so checks compare plain values.
const lua = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => String(r.result)), code).then((r) => {
    const m = r.match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
    return m ? JSON.parse(`"${m[1]}"`) : r;
  });
async function waitReady(page) {
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
}
const dialogVisible = (page) =>
  page.evaluate(() => {
    const d = document.getElementById("daemon-trust");
    return !!d && d.getClientRects().length > 0 ? d.textContent : null;
  });

// The daemon's config: a marker proving the daemon's init.lua ran in the page VM.
const cfg = mkdtempSync(join(tmpdir(), "bemtvi-trust-cfg-"));
writeFileSync(join(cfg, "init.lua"), "vim.g.daemon_cfg = 1\n");

const daemon = spawn(BEMTVI, ["--daemon", "--listen", "127.0.0.1:0"], {
  stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, BEMTVI_CONFIG: cfg },
});
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
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }
  const u = new URL(uri);
  const hostPort = u.host;
  const cert = new URLSearchParams(u.search).get("cert");
  const localUrl = `http://localhost:${PORT}/web/`;
  const daemonUrl = `${localUrl}?daemon=${encodeURIComponent(uri)}`;

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // ── Local session: a clean slate, then a secret in register `a`, flushed to OPFS ──────────
  await page.goto(localUrl);
  await waitReady(page);
  await page.evaluate(async () => {
    try { await (await navigator.storage.getDirectory()).removeEntry(".bemtvi", { recursive: true }); } catch {}
    localStorage.clear();
  });
  await page.evaluate(() => window.__bemtvi.feed("ggdGiLOCAL-SECRET<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('"ayy'));
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  check("local: register a holds the secret", (await lua(page, "return btv.reg.get('a')")).includes("LOCAL-SECRET"));

  // ── 1. An unapproved link waits for the user ─────────────────────────────────────────────
  await page.goto(daemonUrl);
  await sleep(1500);
  const text = await dialogVisible(page);
  check("unapproved link: a confirm dialog is shown", text !== null);
  check("dialog names HOST:PORT", !!text && text.includes(hostPort), `text=${JSON.stringify(text)}`);
  check("dialog shows the cert hash", !!text && text.includes(cert), `text=${JSON.stringify(text)}`);
  check("unapproved link: no edit-host Worker started (nothing dialed)", page.workers().length === 0, `workers=${page.workers().length}`);
  if (text !== null) await page.click("#daemon-trust-connect");
  await waitReady(page);
  check("confirmed: the daemon is connected", (await lua(page, "return tostring(btv.daemon.status())")) === "connected");
  check("confirmed: the daemon's init.lua ran", (await lua(page, "return tostring(vim.g.daemon_cfg)")) === "1");

  // ── 2. The daemon session can't read the local session's registers ───────────────────────
  const leaked = await lua(page, "return btv.reg.get('a')");
  check("isolation: the daemon session does NOT see the local register a", !leaked.includes("LOCAL-SECRET"), `a=${JSON.stringify(leaked)}`);
  // Nor can the daemon's config reach the local blob through the local-always fs seam
  // (`btv.fs_local` hits OPFS directly) — not by reading it, nor by copying / renaming the
  // directory that holds it out of the way first.
  await lua(page, `
    __probe = {}
    local function try(name, p)
      p:next(function(v) __probe[name] = "OK:" .. tostring(v) end,
             function(e) __probe[name] = "ERR" end)
    end
    try("read", btv.fs_local.read_text("/.bemtvi/shada"))
    try("read_dotdot", btv.fs_local.read_text("/x/../.bemtvi/./shada"))
    try("rename_file", btv.fs_local.rename("/.bemtvi/shada", "/stolen"))
    try("rename_dir", btv.fs_local.rename("/.bemtvi", "/stolen-dir"))
  `);
  for (let i = 0; i < 50; i++) {
    if ((await lua(page, "return tostring(vim.tbl_count(__probe))")) === "4") break;
    await sleep(50);
  }
  const probe = await lua(page, "return vim.json.encode(__probe)");
  check("isolation: btv.fs_local can't read the local shada blob", probe.includes('"read":"ERR"') && probe.includes('"read_dotdot":"ERR"') && !probe.includes("LOCAL-SECRET"), `probe=${probe}`);
  check("isolation: btv.fs_local can't move the shada blob or its directory", probe.includes('"rename_file":"ERR"') && probe.includes('"rename_dir":"ERR"'), `probe=${probe}`);

  // ── 3. The daemon's own state persists; an approved daemon reconnects with no prompt ─────
  await page.evaluate(() => window.__bemtvi.feed("ggdGiDAEMON-REG<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('"byy'));
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await page.reload();
  await sleep(500);
  check("approved daemon: reload connects with no prompt", (await dialogVisible(page)) === null);
  await waitReady(page);
  check("approved daemon: connected", (await lua(page, "return tostring(btv.daemon.status())")) === "connected");
  const own = await lua(page, "return btv.reg.get('b')");
  check("daemon namespace: its own register b survived the reload", own.includes("DAEMON-REG"), `b=${JSON.stringify(own)}`);

  // ── 4. The local session keeps its own state and never sees the daemon's ────────────────
  await page.goto(localUrl);
  await waitReady(page);
  const localA = await lua(page, "return btv.reg.get('a')");
  const localB = await lua(page, "return btv.reg.get('b')");
  check("local: register a is intact", localA.includes("LOCAL-SECRET"), `a=${JSON.stringify(localA)}`);
  check("local: the daemon's register b is not visible", !localB.includes("DAEMON-REG"), `b=${JSON.stringify(localB)}`);

  // ── 5a. A different cert at the same address prompts again ───────────────────────────────
  const other = cert.split(":").map((h, i) => (i === 0 ? (h === "00" ? "01" : "00") : h)).join(":");
  const tamperedUri = uri.replace(cert, other);
  await page.goto(`${localUrl}?daemon=${encodeURIComponent(tamperedUri)}`);
  await sleep(1000);
  check("different cert, same address: prompts again", (await dialogVisible(page)) !== null);

  // ── 5b. Declining keeps the page serverless with a visible note ──────────────────────────
  const fresh = await browser.newContext();
  const p2 = await fresh.newPage();
  await p2.goto(daemonUrl);
  await sleep(1000);
  check("fresh profile: the link prompts", (await dialogVisible(p2)) !== null);
  if ((await dialogVisible(p2)) !== null) await p2.click("#daemon-trust-local");
  await waitReady(p2);
  check("declined: no daemon link (serverless)", (await lua(p2, "return tostring(btv.daemon.status())")) === "nil");
  check("declined: the daemon's init.lua did not run", (await lua(p2, "return tostring(vim.g.daemon_cfg)")) === "nil");
  const note = await p2.evaluate(() => {
    const n = document.getElementById("daemon-note");
    return n && n.getClientRects().length > 0 ? n.textContent : null;
  });
  check("declined: a visible note says the page stayed local", !!note && note.includes(hostPort), `note=${JSON.stringify(note)}`);
  await p2.reload();
  await sleep(1000);
  check("declined: not remembered — the link prompts again on reload", (await dialogVisible(p2)) !== null);
} catch (e) {
  console.error("verify-daemon-trust error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — a ?daemon= link connects only after approval, and its shada is isolated per daemon"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
