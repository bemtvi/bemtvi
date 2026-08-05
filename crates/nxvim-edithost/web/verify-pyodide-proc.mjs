// Playwright verifier for the LOCAL (serverless) async-proc leg — `vim.system` / `jobstart`
// fulfilled by the in-browser python interpreter (Pyodide) with NO daemon and NO server
// (Phase 3 of docs/plans/2026-06-23-web-python-demo.md). The terminal leg (Phase 1/2) runs an
// interactive child through one merged PTY stream; the proc leg instead runs a child to
// completion and hands back its stdout, stderr, and exit code SEPARATELY — the contract
// `nx.run` (and `vim.system`) promise. `nx.run_stream` exercises the streaming sibling.
//
// Faithfulness (not a no-op): every result here is one only a real interpreter could produce —
// `nx.run{python -c "…"}` computes a sum and prints it to stdout while writing a distinct line to
// stderr, a `sys.exit(3)` surfaces as code 3, a missing binary is command-not-found (127), stdin
// is piped into the child and echoed back, and a streaming run delivers its lines through
// `nx.run_stream`. No wire, no daemon: a static page running CPython for `vim.system`.
//
// Runs against the **python-demo** site (build-demo.sh → demo-site/), where the local Pyodide
// host is installed (build-config localHost:true). Prereqs: ./build-demo.sh, and a Chromium for
// Playwright (PW_CHROMIUM=/path/to/chrome on macOS). Run: node verify-pyodide-proc.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8149;

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

const SEP = ""; // a separator that can't appear in the child's text output

// `execLua` renders the Lua return as `"ok:<value>"` / `"err:<msg>"`: an integer renders cleanly
// (`"ok:5050"`), but a STRING is Rust-Debug-wrapped (`ok:String(Utf8String { s: Ok("…") })`) with
// newlines escaped to a literal `\n`. A nil return → `"ok:Nil"`, which our readouts use as a
// "not ready yet" sentinel (the async result hasn't landed). So we tag the fields into one string
// and substring-match — the Debug wrapper is irrelevant to a presence/regex check.
function notReady(result) {
  if (result == null) return true;
  const s = String(result);
  return s.startsWith("err:") || s.slice(3) === "Nil";
}

// Run a one-shot `nx.run{…}` (the Lua expression is spliced in) and poll for its resolved result,
// returned as { code, stdout, stderr } (stdout/stderr are the Debug-wrapped renderings — match
// substrings, not exact equality). The first call also pays Pyodide's one-time load, so the budget
// is generous. Returns null on timeout so a failure shows what (didn't) happen.
async function runOneShot(page, key, runExpr, ms = 60000) {
  await page.evaluate(
    ({ key, runExpr }) =>
      window.__nxvim.execLua(`
        _G.${key} = nil
        ;(${runExpr}):next(function(r)
          _G.${key} = { code = r.code, stdout = r.stdout or "", stderr = r.stderr or "" }
        end, function(e)
          _G.${key} = { code = -999, stdout = "", stderr = tostring(e) }
        end)
        return true
      `),
    { key, runExpr },
  );
  const start = Date.now();
  for (;;) {
    const got = await page
      .evaluate(
        (key) =>
          window.__nxvim
            .execLua(
              `local t = _G.${key}
               if t == nil then return nil end
               return "CODE<" .. tostring(t.code) .. ">OUT<" .. t.stdout .. ">ERR<" .. t.stderr .. ">END"`,
            )
            .then((r) => r.result),
        key,
      );
    if (!notReady(got)) {
      const raw = String(got);
      const code = (raw.match(/CODE<(-?\d+)>/) || [])[1];
      const stdout = (raw.match(/OUT<([\s\S]*)>ERR</) || [])[1] ?? "";
      const stderr = (raw.match(/>ERR<([\s\S]*)>END/) || [])[1] ?? "";
      return { code: code != null ? Number(code) : NaN, stdout, stderr };
    }
    if (Date.now() - start > ms) return null;
    await sleep(150);
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

  // SERVERLESS: no `?daemon=` — there is no backend at all.
  await page.goto(`http://localhost:${PORT}/web/`);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (serverless, SAB transport)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted serverless (window.__nxvim.ready resolved, no daemon)", true);

  // ── 1) one-shot nx.run: compute on stdout, a distinct line on stderr, clean exit 0. The sum is
  // one only a real interpreter produces; stdout and stderr come back as SEPARATE strings. ──────
  const code1 = "import sys; print('SUM', sum(range(101))); sys.stderr.write('ERRLINE\\n')";
  const r1 = await runOneShot(page, "P1", `nx.run({ cmd = "python", args = { "-c", ${JSON.stringify(code1)} } })`);
  check("nx.run: the in-browser python computed sum(0..100)=5050 on stdout",
    !!r1 && /SUM 5050/.test(r1.stdout), `r1=${JSON.stringify(r1)}`);
  check("nx.run: stderr is captured SEPARATELY from stdout",
    !!r1 && /ERRLINE/.test(r1.stderr) && !/ERRLINE/.test(r1.stdout), `r1=${JSON.stringify(r1)}`);
  check("nx.run: a clean run exits 0", !!r1 && r1.code === 0, `r1=${JSON.stringify(r1)}`);

  // ── 2) a non-zero exit code propagates (sys.exit(3)). ────────────────────────────────────────
  const r2 = await runOneShot(page, "P2", `nx.run({ cmd = "python", args = { "-c", "import sys; sys.exit(3)" } })`);
  check("nx.run: sys.exit(3) surfaces as code 3", !!r2 && r2.code === 3, `r2=${JSON.stringify(r2)}`);

  // ── 3) a python traceback → exit 1 with the error on stderr. ─────────────────────────────────
  const r3 = await runOneShot(page, "P3", `nx.run({ cmd = "python", args = { "-c", "raise ValueError('boom')" } })`);
  check("nx.run: an uncaught exception exits 1 with the traceback on stderr",
    !!r3 && r3.code === 1 && /ValueError: boom/.test(r3.stderr), `r3=${JSON.stringify(r3)}`);

  // ── 4) a missing binary is command-not-found (127), like a shell — not a host crash. ─────────
  const r4 = await runOneShot(page, "P4", `nx.run({ cmd = "git", args = { "status" } })`);
  check("nx.run: a non-python binary is command-not-found (exit 127)",
    !!r4 && r4.code === 127 && /command not found: git/.test(r4.stderr), `r4=${JSON.stringify(r4)}`);

  // ── 5) stdin is piped into the child and read back (uppercased). ─────────────────────────────
  const code5 = "import sys; print(sys.stdin.read().strip().upper())";
  const r5 = await runOneShot(
    page, "P5",
    `nx.run({ cmd = "python", args = { "-c", ${JSON.stringify(code5)} }, stdin = "hello stdin" })`,
  );
  check("nx.run: the child's stdin was piped through (read + uppercased)",
    !!r5 && /HELLO STDIN/.test(r5.stdout) && r5.code === 0, `r5=${JSON.stringify(r5)}`);

  // ── 6) nx.run_stream: stdout arrives as line batches, then end-of-stream. ────────────────────
  const code6 = "for i in range(5): print('LINE', i)";
  await page.evaluate(
    (code) =>
      window.__nxvim.execLua(`
        _G.SLINES = {}
        _G.SDONE = false
        nx.async(function()
          local st = nx.run_stream({ cmd = "python", args = { "-c", ${JSON.stringify(code)} } })
          for batch in nx.await_each(st) do
            for _, l in ipairs(batch) do _G.SLINES[#_G.SLINES + 1] = l end
          end
          _G.SDONE = true
        end)()
        return true
      `),
    code6,
  );
  let streamed = "";
  for (let i = 0; i < 400; i++) {
    const done = await page.evaluate(() =>
      window.__nxvim
        .execLua('if _G.SDONE then return "LINES<" .. table.concat(_G.SLINES, "|") .. ">END" else return nil end')
        .then((r) => r.result));
    if (!notReady(done)) { streamed = (String(done).match(/LINES<([\s\S]*)>END/) || [])[1] ?? ""; break; }
    await sleep(150);
  }
  const streamedLines = streamed.split("|").filter((l) => /^LINE \d/.test(l));
  check("nx.run_stream: all five streamed stdout lines arrived (LINE 0..4)",
    /LINE 0/.test(streamed) && /LINE 4/.test(streamed) && streamedLines.length === 5,
    `streamed=${JSON.stringify(streamed)}`);

  await browser.close();
} catch (e) {
  console.error("verify-pyodide-proc error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless `vim.system`/`nx.run` runs CPython (Pyodide) in-browser: stdout/stderr/exit captured, streaming works"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
