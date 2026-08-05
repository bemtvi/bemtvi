// Playwright verifier for REMOTE CONFIG & PLUGINS on the browser edit-host: a real
// `nxvim --daemon --listen` ships its *own* config + plugins over WebTransport, and the
// browser editor is born remote — it stages the daemon's config tree into its in-memory FS,
// points the runtimepath at the copy, and sources `init.lua` + plugins, exactly as a native
// edit-host session does (the web twin of the daemon side of
// docs/plans/2026-06-23-remote-config-and-plugins.md).
//
// The proof that config came from the DAEMON, not the local browser origin: the daemon's
// `NXVIM_CONFIG` dir lives on Node's disk (the page origin can't read it), and the page is
// opened with NO local OPFS `/init.lua`. So three globals can only be set if the daemon's
// config surface loaded over the wire:
//   • vim.g.remote_opt    ← the daemon's init.lua ran            (config sourced)
//   • vim.g.remote_required ← require("remote_mod") resolved      (lua/ module + package.path)
//   • vim.g.remote_plugin ← a pack/*/start/* plugin's plugin/ script ran (plugin load)
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-remote-config.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
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
async function until(page, fn, pred, ms = 6000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Read an integer Lua expression back through exec_lua (renders an int plainly, e.g. "13").
async function evalInt(page, expr) {
  return page.evaluate((e) => window.__nxvim.execLua("return " + e).then((r) => String(r.result)), expr);
}

// ── The daemon's config dir (on real disk; NXVIM_CONFIG points the daemon at it) ───────────
const cfg = mkdtempSync(join(tmpdir(), "nxvim-remote-cfg-"));
// init.lua: a distinctive option + a `require` of a lua/ module under this same config dir.
writeFileSync(
  join(cfg, "init.lua"),
  [
    "vim.o.tabstop = 13",
    "vim.g.remote_opt = vim.o.tabstop",
    'local m = require("remote_mod")',
    "vim.g.remote_required = m.value",
  ].join("\n"),
);
// lua/remote_mod.lua: a require-able module (resolves only if package.path was seeded from
// the rebased runtimepath, i.e. the config dir crossed the wire and was staged locally).
mkdirSync(join(cfg, "lua"));
writeFileSync(join(cfg, "lua", "remote_mod.lua"), "return { value = 7 }");
// A package plugin under pack/*/start/* — its plugin/ script must be sourced at startup.
const plug = join(cfg, "pack", "demo", "start", "greeter", "plugin");
mkdirSync(plug, { recursive: true });
writeFileSync(join(plug, "greeter.lua"), "vim.g.remote_plugin = 99");

// ── Spawn the real daemon with NXVIM_CONFIG → the dir above; parse its connect URI ─────────
const daemon = spawn(NXVIM, ["--daemon", "--listen", "127.0.0.1:0"], {
  stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, NXVIM_CONFIG: cfg },
});
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

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted + dialed the daemon (window.__nxvim.ready resolved)", true);

  // ── 1. The daemon's init.lua ran: a distinctive option took effect ────────────────────
  const opt = await until(page, () => window.__nxvim.execLua("return vim.g.remote_opt").then((r) => String(r.result)), (v) => /13$/.test(v));
  check("config: the daemon's init.lua set vim.o.tabstop = 13", /13$/.test(opt), `remote_opt=${JSON.stringify(opt)}`);

  // ── 2. require("remote_mod") resolved against the staged lua/ tree ─────────────────────
  const req = await evalInt(page, "vim.g.remote_required");
  check("require: a lua/ module from the daemon's config resolved (value 7)", /7$/.test(req), `remote_required=${JSON.stringify(req)}`);

  // ── 3. The pack/*/start/* plugin's plugin/ script was sourced ─────────────────────────
  const plugv = await evalInt(page, "vim.g.remote_plugin");
  check("plugin: a pack/*/start/* plugin from the daemon loaded (value 99)", /99$/.test(plugv), `remote_plugin=${JSON.stringify(plugv)}`);

  await browser.close();
} catch (e) {
  console.error("verify-remote-config error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host runs the daemon's config + plugins, fetched over WebTransport"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
