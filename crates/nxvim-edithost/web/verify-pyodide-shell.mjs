// Playwright verifier for the minimal POSIX shell behind a bare `:terminal` (Phase 9 of
// docs/plans/2026-06-23-web-python-demo.md). Runs against the demo site (build-demo.sh →
// demo-site/), where bare `:terminal` opens the in-browser shell (`:terminal python …` stays the
// REPL/script). It drives real shell command lines and asserts:
//
//   - the shell opens (banner + a `<cwd> $ ` prompt) after Pyodide loads;
//   - `pwd` / `ls` see the seeded project (main.py, geometry.py, …);
//   - a redirect (`echo … > f`) + `cat f` round-trips AND the bytes persist to OPFS (syncfs);
//   - a pipeline with a python stage threads stdin (`echo … | python -c …`);
//   - `mkdir` + `cd` change the cwd (prompt updates) and the dir lands in OPFS;
//   - `export FOO=…` + `$FOO` expansion; an unknown command fails loud (command not found);
//   - `python main.py` runs the seeded project from the shell.
//
// Prereqs: ./build-demo.sh (assembles demo-site/) and a Chromium for Playwright. Run:
//   node verify-pyodide-shell.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8125;
const DEMO_SITE = `${here}../demo-site`;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const READ_BUF = 'return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\\n")';
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

async function readOpfs(page, path) {
  return page.evaluate(async (p) => {
    try {
      let dir = await navigator.storage.getDirectory();
      const parts = p.split("/").filter(Boolean);
      const name = parts.pop();
      for (const d of parts) dir = await dir.getDirectoryHandle(d);
      return await (await (await dir.getFileHandle(name)).getFile()).text();
    } catch { return null; }
  }, path);
}

async function opfsHasDir(page, path) {
  return page.evaluate(async (p) => {
    try {
      let dir = await navigator.storage.getDirectory();
      for (const d of p.split("/").filter(Boolean)) dir = await dir.getDirectoryHandle(d);
      return true;
    } catch { return false; }
  }, path);
}

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
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);

  // feed a shell line and wait for `re` to appear in the terminal buffer.
  const run = async (line, re) => {
    await page.evaluate((k) => window.__nxvim.feed(k), `${line}<CR>`);
    return waitFor(page, re);
  };

  // Open the shell (bare :terminal). First run loads Pyodide.
  await page.evaluate(() => window.__nxvim.feed(":terminal<CR>"));
  const banner = await waitFor(page, /\/ \$ $/m);
  check("shell: opens with a prompt after Pyodide loads", /nxvim shell/.test(banner), `buf=${JSON.stringify(banner.slice(-80))}`);

  // 1. pwd → "/"
  const pwd = await run("pwd", /\n\/\n/);
  check("shell: pwd prints /", /\n\/\n/.test(pwd));

  // 2. ls sees the seeded project.
  const ls = await run("ls", /main\.py/);
  check("shell: ls lists the seeded project (main.py, geometry.py, TOUR.md)",
    /main\.py/.test(ls) && /geometry\.py/.test(ls) && /TOUR\.md/.test(ls), `buf=${JSON.stringify(ls.slice(-120))}`);

  // 3. redirect + cat round-trip, and the bytes persist to OPFS.
  await run("echo hi there > note.txt", /\$ $/m);
  const cat = await run("cat note.txt", /\nhi there\n/);
  check("shell: echo > file then cat reads it back", /\nhi there\n/.test(cat));
  check("shell: the redirected write persisted to OPFS", (await readOpfs(page, "/note.txt")) === "hi there\n",
    `opfs=${JSON.stringify(await readOpfs(page, "/note.txt"))}`);

  // 4. pipeline with a python stage threads stdin.
  const piped = await run('echo hello | python -c "import sys; print(sys.stdin.read().strip().upper())"', /\nHELLO\n/);
  check("shell: a pipe threads stdin into a python stage (echo | python)", /\nHELLO\n/.test(piped));

  // 5. mkdir + cd change the cwd (prompt updates) and the dir lands in OPFS.
  await run("mkdir sub", /\$ $/m);
  const cd = await run("cd sub", /\/sub \$ $/m);
  check("shell: cd updates the prompt cwd (/sub)", /\/sub \$ $/m.test(cd));
  check("shell: mkdir created the directory in OPFS", await opfsHasDir(page, "/sub"));
  await run("cd /", /\n\/ \$ $/m);

  // 6. export + $VAR expansion.
  await run("export GREETING=ahoy", /\$ $/m);
  const expanded = await run("echo $GREETING", /\nahoy\n/);
  check("shell: export + $VAR expansion", /\nahoy\n/.test(expanded));

  // 7. unknown command fails loud.
  const notfound = await run("definitelynotacommand", /command not found/);
  check("shell: an unknown command fails loud (command not found)", /definitelynotacommand: command not found/.test(notfound));

  // 8. run the seeded project from the shell.
  const proj = await run("python main.py", /circle area/);
  check("shell: python main.py runs the seeded project", /circle area/.test(proj), `buf=${JSON.stringify(proj.slice(-120))}`);

  // Clean up the files this test created so a re-run starts clean.
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    for (const n of ["note.txt"]) { try { await root.removeEntry(n); } catch {} }
    try { await root.removeEntry("sub", { recursive: true }); } catch {}
  });
  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — bare `:terminal` opens a minimal shell: builtins, pipes/redirects, $VAR, OPFS-persisted writes, and `python` stages all work in-browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
