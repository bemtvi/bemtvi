// Playwright verifier for the browser edit-host's `nx.git` leg
// (docs/plans/2026-07-24-native-git-gix.md) against a REAL `nxvim --daemon --listen` over
// WebTransport. A browser `nx.git.*` op has no in-browser git engine — the op crosses the wire
// to the daemon as one `git_op` request, runs there through `nxvim_git::run_git_job` against the
// real repo, and its typed result returns and resolves the op's promise in the tick. The browser
// twin of the native off-tick `nx.git` actor leg.
//
// Faithfulness (not a no-op — the daemon repo exists ONLY on the daemon's disk):
//   (1) nx.git.head returns the daemon repo's branch (main);
//   (2) nx.git.show returns a file's HEAD blob from the daemon's object store, NOT the edited
//       working-tree content, proving it read git objects over the wire;
//   (3) nx.git.discover on a non-repo path REJECTS with err.code == "ENOREPO";
//   (4) nx.git.clone (a Phase-2 MUTATION verb) runs daemon-side over the wire and lands a real
//       worktree on the daemon's disk with the committed HEAD content — proving the mutation
//       verbs ride the same git_op leg as the reads;
//   (5) nx.git.status carries its `ignored` OPT-IN over the wire — off, no `!!` entries; on, the
//       ignored file and the collapsed ignored directory both arrive.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p nxvim`
// (target/debug/nxvim), a Chromium for Playwright, and `git` on PATH. Run: node verify-git-op.mjs
import { chromium } from "playwright";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, mkdtempSync, writeFileSync, mkdirSync, existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8143;
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

async function until(page, fn, pred, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__nxvim.execLua(c).then((r) => r.result), code);

// `git` is required to build the fixture (the editor itself never shells out to git).
if (spawnSync("git", ["--version"]).status !== 0) {
  console.log("skip: git not on PATH");
  process.exit(0);
}
function git(cwd, args) {
  const r = spawnSync("git", args, {
    cwd,
    env: {
      ...process.env,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      GIT_AUTHOR_NAME: "nxvim test",
      GIT_AUTHOR_EMAIL: "test@nxvim",
      GIT_COMMITTER_NAME: "nxvim test",
      GIT_COMMITTER_EMAIL: "test@nxvim",
    },
  });
  if (r.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${r.stderr}`);
}

// ── The daemon's repo (real disk; the browser queries it OVER THE WIRE) ──────────────────────
const root = mkdtempSync(join(tmpdir(), "nxvim-gitop-"));
git(root, ["init", "-q", "-b", "main"]);
const trackedFile = join(root, "file.txt");
writeFileSync(trackedFile, "a\nb\nc\n");
git(root, ["add", "-A"]);
git(root, ["commit", "-q", "-m", "initial"]);
// Diverge the working tree from HEAD so `show` proving it read the object store is meaningful.
writeFileSync(trackedFile, "EDITED-IN-WORKTREE\n");
// A committed .gitignore plus an ignored file + a wholly-ignored directory, for the
// `status { ignored = true }` check: the flag is a field on the status job, so this proves the
// wire carries it (without it the daemon walks with ignored pruned and reports neither).
writeFileSync(join(root, ".gitignore"), "*.log\nbuild\n");
git(root, ["add", ".gitignore"]);
git(root, ["commit", "-q", "-m", "ignore"]);
writeFileSync(join(root, "noise.log"), "noise\n");
mkdirSync(join(root, "build"), { recursive: true });
writeFileSync(join(root, "build", "a.o"), "x");
writeFileSync(join(root, "build", "b.o"), "x");
// A non-repo directory (outside the repo) for the ENOREPO reject.
const noRepo = mkdtempSync(join(tmpdir(), "nxvim-norepo-"));
// A fresh (non-existent) destination the browser will clone the daemon repo INTO, over the
// wire — the clone runs daemon-side, so it lands on the daemon's disk (this process's disk).
const cloneDir = join(mkdtempSync(join(tmpdir(), "nxvim-gitclone-")), "cloned");

// ── Spawn the real daemon (cwd = the repo) ───────────────────────────────────────────────────
const daemon = spawn(NXVIM, ["--daemon", "--listen", "127.0.0.1:0"], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
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

  // ── 1. head: the daemon repo's branch round-trips over the wire ──────────────────────────
  await luaResult(page, `_G.__b, _G.__berr = nil, nil
     nx.git.head("${trackedFile}"):next(
       function(h) _G.__b = h.branch end,
       function(e) _G.__berr = e.code end)
     return 1`);
  const branch = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__b or _G.__berr)").then((r) => r.result),
    (v) => /main|E[A-Z]/.test(String(v)));
  check("nx.git.head returns the daemon repo's branch (over WebTransport)",
    /main/.test(String(branch)), `branch=${JSON.stringify(branch)}`);

  // ── 2. show: the HEAD blob from the daemon's object store, NOT the edited working tree ────
  await luaResult(page, `_G.__s, _G.__serr = nil, nil
     nx.git.show("${trackedFile}", "HEAD"):next(
       function(bytes) _G.__s = bytes end,
       function(e) _G.__serr = e.code end)
     return 1`);
  const blob = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__s or _G.__serr)").then((r) => r.result),
    (v) => /a\\nb\\nc|EDITED|E[A-Z]/.test(String(v)));
  check("nx.git.show returns the HEAD blob from the daemon's object store (not the worktree)",
    /a\\nb\\nc/.test(String(blob)) && !/EDITED/.test(String(blob)), `blob=${JSON.stringify(blob)}`);

  // ── 3. discover outside a repo REJECTS with err.code == "ENOREPO" over the wire ───────────
  await luaResult(page, `_G.__code = nil
     nx.git.discover("${noRepo}"):next(
       function(_) _G.__code = "RESOLVED?!" end,
       function(e) _G.__code = e.code end)
     return 1`);
  const code = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__code)").then((r) => r.result),
    (v) => !/^nil$/.test(String(v)));
  check("nx.git.discover outside a repo rejects with err.code == ENOREPO over the wire",
    /ENOREPO/.test(String(code)), `code=${JSON.stringify(code)}`);

  // ── 4. clone (a MUTATION verb) runs daemon-side, landing a real worktree over the wire ────
  await luaResult(page, `_G.__c, _G.__cerr = nil, nil
     nx.git.clone("${root}", "${cloneDir}"):next(
       function(dir) _G.__c = dir end,
       function(e) _G.__cerr = e.code end)
     return 1`);
  const cloned = await until(page,
    () => window.__nxvim.execLua("return tostring(_G.__c or _G.__cerr)").then((r) => r.result),
    (v) => !/^nil$/.test(String(v)), 20000);
  const fileOnDisk = existsSync(join(cloneDir, "file.txt"));
  // The clone checked out HEAD (committed "a\nb\nc\n"), NOT the daemon repo's edited worktree.
  const content = fileOnDisk ? readFileSync(join(cloneDir, "file.txt"), "utf8") : "";
  check("nx.git.clone (mutation verb) resolves over the wire + lands the worktree on the daemon disk",
    fileOnDisk && content === "a\nb\nc\n" && !/E[A-Z]/.test(String(cloned)),
    `cloned=${JSON.stringify(cloned)} fileOnDisk=${fileOnDisk} content=${JSON.stringify(content)}`);

  // ── 5. status { ignored = true }: the opt-in flag crosses the wire ────────────────────────
  // `opts.ignored` is a field on the status job; a codec that drops it silently degrades a
  // browser session to "no ignored paths" — a feature that works natively and not remotely.
  // The wholly-ignored `build/` must arrive COLLAPSED (one entry, not one per .o file), which
  // is what makes this affordable for a file tree.
  const statusCall = (opts) => `_G.__st, _G.__sterr = nil, nil
     nx.git.status("${root}"${opts}):next(
       function(r)
         local out = {}
         for _, e in ipairs(r.entries) do out[#out + 1] = e.path .. "=" .. e.index .. e.worktree end
         table.sort(out)
         _G.__st = table.concat(out, ",")
       end,
       function(e) _G.__sterr = e.code end)
     return 1`;
  const readStatus = () => until(page,
    () => window.__nxvim.execLua("return tostring(_G.__st or _G.__sterr)").then((r) => r.result),
    (v) => !/^nil$/.test(String(v)), 15000);

  await luaResult(page, statusCall(""));
  const plain = String(await readStatus());
  check("nx.git.status default does NOT report ignored paths over the wire",
    /file\.txt= M/.test(plain) && !/!!/.test(plain), `status=${JSON.stringify(plain)}`);

  await luaResult(page, statusCall(", { ignored = true }"));
  const withIgnored = String(await readStatus());
  check("nx.git.status { ignored = true } crosses the wire (build/ collapsed to one !! entry)",
    /noise\.log=!!/.test(withIgnored) &&
      /build=!!/.test(withIgnored) &&
      !/build\/a\.o/.test(withIgnored),
    `status=${JSON.stringify(withIgnored)}`);

  await browser.close();
} catch (e) {
  console.error("verify-git-op error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser nx.git runs on a real nxvim --daemon over WebTransport (head, show-from-object-store, ENOREPO reject, clone, status+ignored)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
