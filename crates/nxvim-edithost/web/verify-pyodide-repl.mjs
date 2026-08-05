// Playwright verifier for the interactive in-browser python REPL (Phase 2 of
// docs/plans/2026-06-23-web-python-demo.md) — bare `:terminal python` with NO daemon. The REPL
// is driven by keystrokes through the terminal seam: a host-side line editor accumulates input,
// the Pyodide Worker runs completed statements synchronously via `codeop`, and Ctrl-C interrupts
// a running loop via a SharedArrayBuffer SIGINT.
//
// Faithfulness (not a no-op): real evaluation (`6*7` → 42), the displayhook echoing an
// expression's value, a multiline `def` block (continuation prompt + a body), and a genuine
// interrupt of an infinite `while` loop (the SAB SIGINT reaches CPython mid-loop and raises a
// catchable KeyboardInterrupt, after which the REPL recovers). All serverless, all in-browser.
//
// Runs against the python-demo site (build-demo.sh → demo-site/). Prereqs: ./build-demo.sh, and
// a Chromium for Playwright (PW_CHROMIUM=/path/to/chrome on macOS). Run: node verify-pyodide-repl.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8148;

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

const READ_BUF = 'return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\\n")';
// execLua renders the result as `ok:String(Utf8String { s: Ok("…") })` with newlines escaped;
// unwrap + unescape it back to the real buffer text so newline-anchored regexes work.
async function readBuf(page) {
  const raw = await page.evaluate((c) => window.__nxvim.execLua(c).then((r) => String(r.result)), READ_BUF);
  const m = raw.match(/Ok\("([\s\S]*)"\)\s*\}\)\s*$/);
  if (!m) return raw;
  try { return JSON.parse(`"${m[1]}"`); } catch { return m[1]; }
}

async function waitFor(page, re, ms = 40000) {
  const start = Date.now();
  let last = "";
  for (;;) {
    last = await readBuf(page);
    if (re.test(last)) return last;
    if (Date.now() - start > ms) return last;
    await sleep(100);
  }
}

const DEMO_SITE = `${here}../demo-site`;
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, NXVIM_SERVE_ROOT: DEMO_SITE },
});
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`); // serverless
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("page is cross-origin isolated (serverless, SAB transport)",
    await page.evaluate(() => self.crossOriginIsolated) === true);

  const feed = async (keys, ms = 900) => {
    await page.evaluate((k) => window.__nxvim.feed(k), keys);
    await sleep(ms);
  };

  // Open the REPL (bare `python`) and wait for the prompt (first run loads Pyodide).
  await page.evaluate(() => window.__nxvim.feed(":terminal python<CR>"));
  await waitFor(page, /^>>> $/m);
  check("repl: the prompt appears after Pyodide loads", /Python .* \(Pyodide\)/.test(await readBuf(page)));

  // 1. Evaluate an expression — the displayhook echoes its value.
  await feed("6*7<CR>");
  check("repl: an expression evaluates and its value echoes (6*7 → 42)",
    /\n42\n/.test(await readBuf(page)), `buf=${JSON.stringify((await readBuf(page)).slice(-80))}`);

  // 2. A statement (assignment) is silent; a later print uses the binding.
  await feed('greeting = "hi " * 2<CR>');
  await feed("print(greeting.strip())<CR>");
  check("repl: state persists across lines (a binding, then print)",
    /\nhi hi\n/.test(await readBuf(page)));

  // 3. A multiline block: `def` → continuation prompt, a body, a blank line, then call it.
  await feed("def dbl(n):<CR>");
  const contSeen = /\.\.\. /.test(await readBuf(page)); // the `... ` continuation prompt
  await feed("    return n + n<CR>");
  await feed("<CR>"); // blank line ends the block
  await feed("dbl(21)<CR>");
  check("repl: a multiline def block (continuation prompt) defines and runs (dbl(21) → 42)",
    contSeen && /\n42\n/.test((await readBuf(page)).slice(-120)),
    `cont=${contSeen}`);

  // 4. Interrupt an infinite loop with Ctrl-C — the SAB SIGINT raises a catchable
  //    KeyboardInterrupt mid-loop, and the REPL recovers (a later expression still evaluates).
  await feed("while True: pass<CR>", 2000); // runs (compiles complete on one line); now spinning
  // Poll for the interrupt, re-pressing Ctrl-C — a single SIGINT can race the loop's start.
  let afterInt = "";
  for (let i = 0; i < 12 && !/KeyboardInterrupt/.test(afterInt); i++) {
    await page.evaluate(() => window.__nxvim.feed("<C-c>"));
    afterInt = await waitFor(page, /KeyboardInterrupt/, 2000);
  }
  check("repl: Ctrl-C interrupts a running loop (KeyboardInterrupt)",
    /KeyboardInterrupt/.test(afterInt), `tail=${JSON.stringify(afterInt.slice(-120))}`);
  await feed("13+29<CR>");
  check("repl: the REPL recovers after the interrupt (13+29 → 42)",
    /\n42\n/.test((await readBuf(page)).slice(-60)));

  await browser.close();
} catch (e) {
  console.error("verify-pyodide-repl error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless interactive python REPL in-browser: eval, persistent state, multiline, Ctrl-C interrupt"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
