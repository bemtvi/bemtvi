// Playwright verifier for the `.gitignore`-respecting pickers on the pure web client
// (serverless, no daemon). `files_git` / `live_grep_git` fall back from `rg` to `git` and
// finally to the `btv.fs` walk; here there is no process host, so both spawns fail loud
// (code -1) and the walk must honour the ignore files itself (`btv.fs.walk{ gitignore }`,
// reading `.gitignore` over OPFS). Without that the pickers either fail or — worse — list
// the ignored files they promise to drop.
//
// Faithfulness (not a no-op): the repository is seeded through the SAME btv.fs/OPFS seam
// the walk reads, the pickers run their real async sources through the production tick,
// and the assertions read each picker's live candidate list (`btv._picker.items`). The
// unrestricted `files` over the same tree is the control: it must list the ignored file,
// so its absence from `files_git` is the ignore rules, not a seeding failure.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-gitignore-serverless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8177;

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

// Run a chain that stashes its outcome in `_G.<g>`, then poll until that global settles.
async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__bemtvi.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// The open picker's candidate paths once `want` of them have landed (or the timeout).
async function pickerPaths(page, want, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = String(await luaResult(
      page,
      `local p = btv._picker\n` +
        `if not p then return "" end\n` +
        `local t = {}\n` +
        `for i = 1, (p.nitems or 0) do t[#t + 1] = p.items[i].path or p.items[i].text end\n` +
        `table.sort(t)\n` +
        `return table.concat(t, ",")`,
    ));
    const m = v.match(/Ok\("([^"]*)"\)/);
    const s = m ? m[1] : v;
    if (s !== "" && s.split(",").length >= want) return s;
    if (Date.now() - start > ms) return s;
    await sleep(60);
  }
}

const ROOT_REL = `gitignoretest-${Date.now()}`;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
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

  // No `?daemon=` — serverless: rg and git cannot run.
  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted (serverless — no daemon, no process host)", true);

  // A repository (a `.git` makes it one) ignoring a directory and a pattern, plus a
  // dotfile that is NOT ignored — every file holding the needle.
  const seeded = await settle(page, "__seed", `btv.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       btv.await(btv.fs.mkdir(base .. "/.git", { recursive = true }))
       btv.await(btv.fs.mkdir(base .. "/ignored_dir", { recursive = true }))
       btv.await(btv.fs.write(base .. "/.gitignore", "ignored_dir/\\n*.log\\n"))
       for _, f in ipairs({ "ignored_dir/x.txt", "debug.log", "keep.txt", ".hidden.txt" }) do
         btv.await(btv.fs.write(base .. "/" .. f, "zqxneedle\\n"))
       end
       btv.cmd("cd " .. base)
       _G.__seed = "ok"
     end)()`);
  check("seeded a repository with a .gitignore under cwd via btv.fs (OPFS)", /ok/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  // The error shape the walk branches on: `.git/info/exclude` is absent with its parent
  // directory absent too, which OPFS signals as a raw `NotFoundError` DOMException. It must
  // reach Lua as `ENOENT` (as native and the daemon report it) — any other code makes the
  // walk reject instead of reading "no rules", and every serverless `_git` picker comes up empty.
  const missing = await settle(page, "__missing", `btv.async(function()
       local ok, e = pcall(btv.await, btv.fs.read(vim.fn.getcwd() .. "/.git/info/exclude"))
       _G.__missing = ok and "resolved" or tostring(type(e) == "table" and e.code or e)
     end)()`);
  check(
    "a read under a missing OPFS directory rejects ENOENT, as native and the daemon do",
    /ENOENT/.test(String(missing)),
    `code=${JSON.stringify(missing)}`,
  );

  // The control: the unrestricted picker lists the ignored files.
  await luaResult(page, `btv.picker.open('files')`);
  const all = await pickerPaths(page, 5);
  check(
    "files (unrestricted) lists the ignored files too",
    all === ".gitignore,.hidden.txt,debug.log,ignored_dir/x.txt,keep.txt",
    `paths=${all}`,
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);

  await luaResult(page, `btv.picker.open('files_git')`);
  const git = await pickerPaths(page, 3);
  check(
    "files_git walks over btv.fs and drops what .gitignore ignores (dotfile kept)",
    git === ".gitignore,.hidden.txt,keep.txt",
    `paths=${git}`,
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);

  await luaResult(page, `btv.picker.open('live_grep_git', { query = 'zqxneedle' })`);
  const hits = await pickerPaths(page, 2);
  check(
    "live_grep_git searches the same set over btv.fs.grep",
    hits === ".hidden.txt,keep.txt",
    `paths=${hits}`,
  );
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));

  // A submodule's `.git` is a FILE naming its git dir RELATIVE to it
  // (`gitdir: ../.git/modules/sub`), so the walk reads `sub/../.git/modules/sub/…`. OPFS
  // has no `..` entry (a handle lookup by that name throws a TypeError), so the path must
  // resolve `..` lexically — as it does natively and over a daemon — or the walk rejects
  // with EIO and the `_git` pickers list nothing. The module's `info/exclude` applies.
  const sub = await settle(page, "__sub", `btv.async(function()
       local base = vim.fn.getcwd()
       btv.await(btv.fs.mkdir(base .. "/.git/modules/sub/info", { recursive = true }))
       btv.await(btv.fs.write(base .. "/.git/modules/sub/info/exclude", "secret.txt\\n"))
       btv.await(btv.fs.mkdir(base .. "/sub", { recursive = true }))
       btv.await(btv.fs.write(base .. "/sub/.git", "gitdir: ../.git/modules/sub\\n"))
       btv.await(btv.fs.write(base .. "/sub/secret.txt", "x\\n"))
       btv.await(btv.fs.write(base .. "/sub/kept.txt", "x\\n"))
       local ok, files = pcall(btv.await, btv.fs.walk(base .. "/sub", { gitignore = true }))
       if not ok then
         _G.__sub = "ERR:" .. tostring(type(files) == "table" and (files.message or files.code) or files)
         return
       end
       table.sort(files)
       _G.__sub = table.concat(files, ",")
     end)()`);
  check(
    "a submodule's relative gitdir: resolves over OPFS and its info/exclude applies",
    /"kept\.txt"/.test(String(sub)),
    `walk=${JSON.stringify(sub)}`,
  );
  await settle(page, "__rm",`btv.fs.remove(vim.fn.getcwd(), { recursive = true })
       :next(function() _G.__rm = "gone" end, function(e) _G.__rm = "err:" .. e.code end)`);
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless .gitignore pickers honour the ignore files over OPFS"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
