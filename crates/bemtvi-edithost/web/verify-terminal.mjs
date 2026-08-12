// Playwright verifier for the browser edit-host's TERMINAL leg (Phase 7) against a REAL
// `bemtvi --daemon --listen` over WebTransport. The browser `:terminal` has no local PTY — the
// vt100 emulation runs in the browser (the shared EditHost) but the real child runs on the
// daemon: `btv.terminal.open` ships a `term_open` over the wire, the daemon spawns the PTY, and
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
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi), and a Chromium for Playwright. Run:  node verify-terminal.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8142;
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

const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

// `execLua` returns the result rendered as `ok:String(Utf8String { s: Ok("…") })`; pull an
// integer out of that wrapper (the harness's results are debug-rendered, not clean values).
const luaInt = async (page, expr) => {
  const r = String(await luaResult(page, `return tostring(${expr})`));
  const m = r.match(/Ok\("(-?\d+)"\)/);
  return m ? Number(m[1]) : NaN;
};

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

// The computed CSS `color` of the rendered grid span carrying `text` — how the page
// actually paints it (vt100 color → server palette → `renderLineServer`). Polls until it
// matches `want` (the render is async after the PTY output lands), else returns the last seen.
async function pollColor(page, text, want, ms = 4000) {
  const start = Date.now();
  let last = null;
  for (;;) {
    last = await page.evaluate((t) => {
      const grid = document.querySelector("#grid");
      if (!grid) return null;
      for (const sp of grid.querySelectorAll("span")) {
        if (sp.textContent && sp.textContent.includes(t)) return getComputedStyle(sp).color;
      }
      return null;
    }, text);
    if (last === want) return last;
    if (Date.now() - start > ms) return last;
    await sleep(50);
  }
}

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

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted + dialed the daemon (window.__bemtvi.ready resolved)", true);

  // ── 1. output: a child's stdout streams back over the wire and renders in the buffer ──────
  // `btv.terminal.open` is the real control surface (the API twin of `:terminal`); it queues the
  // open the tick forwards as `term_open`. The daemon spawns the PTY and streams `term_data`
  // back. `sleep 1` keeps the child alive long enough to observe the output before its exit.
  await luaResult(page, `btv.terminal.open{ cmd = { "sh", "-c", "printf hello-from-daemon-term; sleep 1" } } return 1`);
  const out = await pollBuf(page, /hello-from-daemon-term/);
  check("terminal: a daemon child's stdout streams over WebTransport into the rendered buffer",
    /hello-from-daemon-term/.test(out), `buf=${JSON.stringify(out)}`);

  // Its exit crosses back too: the `[Process exited 0]` notice appears (term_exit landed).
  const exited = await pollBuf(page, /\[Process exited 0\]/);
  check("terminal: the child's clean exit round-trips (term_exit → [Process exited 0])",
    /\[Process exited 0\]/.test(exited), `buf=${JSON.stringify(exited)}`);

  // ── 1b. color: ANSI SGR colors render as styled cells (vt100 grid → server palette) ───────
  // The terminal's colors live only in the wasm-side vt100 emulator — the JS highlighter can't
  // recover them from buffer text. They must ride the server `highlights`/`styles` palette and
  // paint via `renderLineServer`. `\033[31m` is ANSI red = xterm idx 1 = (205,0,0) = #cd0000.
  // A long-lived child (`sleep 30`, killed at cleanup) keeps the terminal *live* — the real
  // case (a shell emitting color), where the emulator stays mapped and colors project.
  // Build the SGR sequence with an explicit ESC byte (`string.char(27)`) so escaping through
  // JS→execLua→Lua can't mangle it: `<ESC>[31m REDCELL <ESC>[0m`.
  await luaResult(page, `local e=string.char(27); btv.terminal.open{ cmd = { "sh", "-c", "printf '"..e.."[31mREDCELL"..e.."[0m\\n'; sleep 30" } } return 1`);
  await pollBuf(page, /REDCELL/);
  const redColor = await pollColor(page, "REDCELL", "rgb(205, 0, 0)");
  check("terminal: ANSI-colored output renders as a colored span (#cd0000 red)",
    redColor === "rgb(205, 0, 0)", `computed color=${redColor}`);

  // ── 1c. perf: a terminal's (huge) scrollback is NOT shipped through the JS-highlighter line
  // channel. The buffer text reaches the editor (READ_BUF, via the RPC), but the per-frame
  // `lines` blob the UI feeds its tree-sitter highlighter must skip a terminal — re-shipping the
  // whole scrollback on every keystroke echo / output burst was the terminal "slowness".
  const buf = String(await luaResult(page, READ_BUF));
  const shipped = String(await page.evaluate(() => window.__bemtvi.lines()));
  check("terminal: output reaches the buffer but is NOT re-shipped via the full-buffer line channel",
    /REDCELL/.test(buf) && !/REDCELL/.test(shipped),
    `inBuffer=${/REDCELL/.test(buf)} inShippedLines=${/REDCELL/.test(shipped)}`);

  // ── 1b-exit. color persists past the child's exit. The vt100 emulator is dropped when the
  // child dies, but its per-cell colors are frozen into a per-buffer store first, so the dead
  // terminal's final output keeps its highlighting as a plain buffer — `term_colors` stays true
  // (distinct from the now-false `terminal`), so `renderLineServer` still paints it. Here the
  // child prints red then exits immediately; the red span must survive the `[Process exited 0]`.
  await luaResult(page, `local e=string.char(27); btv.terminal.open{ cmd = { "sh", "-c", "printf '"..e.."[31mREDGONE"..e.."[0m\\n'" } } return 1`);
  await pollBuf(page, /REDGONE/);
  await pollBuf(page, /\[Process exited 0\]/);
  const redAfterExit = await pollColor(page, "REDGONE", "rgb(205, 0, 0)");
  check("terminal: a dead terminal keeps its color after the child exits (frozen vt100 colors)",
    redAfterExit === "rgb(205, 0, 0)", `computed color=${redAfterExit}`);

  // ── 1d. cancel: ^C on a flooding terminal trims the scrollback to the tail + a marker ─────
  // A child floods 1000 lines (past the 200-line keep window) then `trap "" INT; cat` keeps it
  // alive (and ignoring SIGINT) so the terminal stays live across the ^C. ^C while flooding
  // trims the mirror to the recent tail and inserts a marker; the earliest lines are dropped.
  await luaResult(page, `btv.terminal.open{ cmd = {'sh','-c','seq 1000; trap "" INT; cat'} } return 1`);
  await pollBuf(page, /1000/);
  const beforeCount = await luaInt(page, "vim.api.nvim_buf_line_count(0)");
  await page.evaluate(() => window.__bemtvi.feed("<C-c>"));
  const trimmed = await pollBuf(page, /earlier lines trimmed/);
  const afterCount = await luaInt(page, "vim.api.nvim_buf_line_count(0)");
  check("terminal: ^C on a flood trims the scrollback to the tail + a marker",
    /earlier lines trimmed/.test(trimmed) && /1000/.test(trimmed) && afterCount < 300 && afterCount < beforeCount,
    `before=${beforeCount} after=${afterCount} marker=${/earlier lines trimmed/.test(trimmed)} tailKept=${/1000/.test(trimmed)}`);

  // ── 1e. cancel: ^C STOPS a continuous flood promptly (drops the in-flight backlog) ────────
  // The real bug: a never-ending flood. End-to-end backpressure bounds the steady state, but the
  // browser's QUIC receive window still holds seconds of already-sent output that keeps arriving
  // after ^C. On a ^C to a flooding terminal the Worker discards that backlog, so output settles
  // in a beat. The child has no SIGINT trap, so ^C also kills it.
  await luaResult(page, `btv.terminal.open{ cmd = {'sh','-c','i=0; while :; do echo floodline-$i; i=$((i+1)); done'} } return 1`);
  await pollBuf(page, /floodline-/);
  await sleep(800); // let a real in-flight backlog build up in the QUIC window
  await page.evaluate(() => window.__bemtvi.feed("<C-c>"));
  // The highest flood index visible in the buffer; output has stopped once it stops rising.
  const maxFloodIdx = async () => {
    const r = String(await luaResult(page, 'return table.concat(vim.api.nvim_buf_get_lines(0,-3,-1,false),"|")'));
    let m = -1;
    for (const x of r.matchAll(/floodline-(\d+)/g)) m = Math.max(m, Number(x[1]));
    return m;
  };
  let prev = -2, stoppedMs = -1;
  const t0 = Date.now();
  for (let i = 0; i < 40; i++) {
    const cur = await maxFloodIdx();
    if (cur === prev) { stoppedMs = Date.now() - t0; break; }
    prev = cur;
    await sleep(150);
  }
  const stoppedCount = await luaInt(page, "vim.api.nvim_buf_line_count(0)");
  check("terminal: ^C stops a continuous flood promptly (drops the in-flight backlog)",
    stoppedMs >= 0 && stoppedMs < 2500 && stoppedCount < 500,
    `stoppedMs=${stoppedMs} bufLines=${stoppedCount}`);

  // ── 2. interactive: input typed into the terminal crosses the wire and the child echoes ───
  // A fresh terminal running `cat`, which echoes its stdin back through the PTY. The open enters
  // terminal mode, so the typed keys are forwarded to the child (`term_write`).
  await luaResult(page, `btv.terminal.open{ cmd = "cat" } return 1`);
  await sleep(300); // let the PTY spawn before typing (term_open must land first)
  await page.evaluate(() => window.__bemtvi.feed("echo-me-back<CR>"));
  const echoed = await pollBuf(page, /echo-me-back/);
  check("terminal: typed input crosses the wire (term_write) and the child's echo streams back",
    /echo-me-back/.test(echoed), `buf=${JSON.stringify(echoed)}`);

  // ── 3. EOF ends it: <C-d> reaches `cat` (term_write), it exits, the notice renders ────────
  await page.evaluate(() => window.__bemtvi.feed("<C-d>"));
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
  ? "\nALL PASS — browser :terminal runs a real PTY on an bemtvi --daemon over WebTransport (output, input echo, exit)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
