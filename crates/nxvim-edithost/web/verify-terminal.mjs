// Playwright verifier for the browser edit-host's TERMINAL leg (Phase 7) against a REAL
// `nxvim --daemon --listen` over WebTransport. The browser `:terminal` has no local PTY — the
// vt100 emulation runs in the browser (the shared EditHost) but the real child runs on the
// daemon: `nx.terminal.open` ships a `term_open` over the wire, the daemon spawns the PTY, and
// its output streams back as `term_data` pushes the Worker feeds to the emulator (the buffer the
// page renders). The browser twin of the native daemon_terminal.rs test.
//
// Faithfulness (not a no-op): (1) a child's stdout streams back over the wire and lands in the
// rendered terminal buffer — text a serverless browser could never produce (it has no process);
// (2) interactive input typed into the terminal crosses the wire (`term_write`), reaches the
// child, and its echo streams back into the buffer; (3) the child's exit crosses back
// (`term_exit`) and the buffer shows the `[Process exited 0]` notice. POSIX commands (sh/cat)
// keep it hermetic.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), and a Chromium for Playwright. Run:  node verify-terminal.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8142;
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

// The whole terminal buffer as one string (history + live screen), for substring assertions.
const READ_BUF = 'return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\\n")';

// Poll the terminal buffer until `re` appears (PTY output is async) — returns the matched
// state, or the last buffer text on timeout so a failure shows what *did* render.
async function pollBuf(page, re, ms = 8000) {
  const start = Date.now();
  let last = "";
  for (;;) {
    last = String(await luaResult(page, READ_BUF));
    if (re.test(last)) return last;
    if (Date.now() - start > ms) return last;
    await sleep(50);
  }
}

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

  // ── 1. output: a child's stdout streams back over the wire and renders in the buffer ──────
  // `nx.terminal.open` is the real control surface (the API twin of `:terminal`); it queues the
  // open the tick forwards as `term_open`. The daemon spawns the PTY and streams `term_data`
  // back. `sleep 1` keeps the child alive long enough to observe the output before its exit.
  await luaResult(page, `nx.terminal.open{ cmd = { "sh", "-c", "printf hello-from-daemon-term; sleep 1" } } return 1`);
  const out = await pollBuf(page, /hello-from-daemon-term/);
  check("terminal: a daemon child's stdout streams over WebTransport into the rendered buffer",
    /hello-from-daemon-term/.test(out), `buf=${JSON.stringify(out)}`);

  // Its exit crosses back too: the `[Process exited 0]` notice appears (term_exit landed).
  const exited = await pollBuf(page, /\[Process exited 0\]/);
  check("terminal: the child's clean exit round-trips (term_exit → [Process exited 0])",
    /\[Process exited 0\]/.test(exited), `buf=${JSON.stringify(exited)}`);

  // ── 2. interactive: input typed into the terminal crosses the wire and the child echoes ───
  // A fresh terminal running `cat`, which echoes its stdin back through the PTY. The open enters
  // terminal mode, so the typed keys are forwarded to the child (`term_write`).
  await luaResult(page, `nx.terminal.open{ cmd = "cat" } return 1`);
  await sleep(300); // let the PTY spawn before typing (term_open must land first)
  await page.evaluate(() => window.__nxvim.feed("echo-me-back<CR>"));
  const echoed = await pollBuf(page, /echo-me-back/);
  check("terminal: typed input crosses the wire (term_write) and the child's echo streams back",
    /echo-me-back/.test(echoed), `buf=${JSON.stringify(echoed)}`);

  // ── 3. EOF ends it: <C-d> reaches `cat` (term_write), it exits, the notice renders ────────
  await page.evaluate(() => window.__nxvim.feed("<C-d>"));
  const catExit = await pollBuf(page, /\[Process exited 0\]/);
  check("terminal: <C-d> reaches the child and its exit ends the terminal ([Process exited 0])",
    /\[Process exited 0\]/.test(catExit), `buf=${JSON.stringify(catExit)}`);

  await browser.close();
} catch (e) {
  console.error("verify-terminal error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser :terminal runs a real PTY on an nxvim --daemon over WebTransport (output, input echo, exit)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
