// Playwright verifier for web session capture (Phase 1 of
// docs/plans/2026-08-14-web-session-restore.md): the window/tab layout a config opted
// into with `btv.shada.save_layout(true)` is captured into the OPFS shada blob, and is
// absent when it did not opt in.
//
// Phase 1 is the persist seam (capture + carry); Phase 2 rebuilds the layout at boot, so
// this asserts both on the stored blob and on the windows that come back after a reload.
//
//   node verify-session.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8129;

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

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "ignore" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 60; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push("pageerror: " + e.message));
  page.on("console", (m) => { if (/config_error|shada/i.test(m.text())) errors.push(m.text()); });

  const boot = async () => {
    await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 30000 });
    await page.evaluate(() => window.__bemtvi.ready);
  };
  const wipeOpfs = () => page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    for await (const [name] of root.entries()) {
      try { await root.removeEntry(name, { recursive: true }); } catch {}
    }
  });
  const writeConfig = (text) => page.evaluate(async (t) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("init.lua", { create: true });
    const w = await fh.createWritable();
    await w.write(t); await w.close();
  }, text);
  const readShada = () => page.evaluate(async () => {
    try {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(".bemtvi");
      const fh = await dir.getFileHandle("shada");
      return await (await fh.getFile()).text();
    } catch { return null; }
  });
  // Open two files and split, so the captured layout has file-backed leaves (a tab with
  // no file-backed window is dropped at capture, by design).
  const buildLayout = async () => {
    await page.evaluate(async () => {
      const root = await navigator.storage.getDirectory();
      // Several lines each, so a restored cursor has somewhere to be other than line 1.
      for (const [n, t] of [["a.txt", "alpha\na2\na3\na4\n"], ["b.txt", "beta\nb2\nb3\nb4\n"]]) {
        const fh = await root.getFileHandle(n, { create: true });
        const w = await fh.createWritable(); await w.write(t); await w.close();
      }
    });
    await page.evaluate(() => window.__bemtvi.feed(":edit /a.txt<CR>"));
    await sleep(500);
    await page.evaluate(() => window.__bemtvi.feed(":split /b.txt<CR>"));
    await sleep(500);
    await page.evaluate(() => window.__bemtvi.feed(":tabnew /c.txt<CR>"));
    await sleep(500);
    await page.evaluate(() => window.__bemtvi.feed("gT")); // back to tab 1 (the split)
    await sleep(400);
    // Park the focused window's cursor off line 1, so the restore has a position to prove.
    await page.evaluate(() => window.__bemtvi.feed("3G"));
    await sleep(300);
  };
  const luaResult = (code) =>
    page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await boot();

  // --- 1. Opted in: the layout is captured into the blob. ---
  await wipeOpfs();
  await writeConfig([
    "btv.shada.save_layout(true)",
    // Counters for check 2c below. They fire freely on THIS boot (the test creates the
    // layout by hand); what matters is the next boot, where only the restore mints windows.
    "vim.g.__winnew, vim.g.__tabnew, vim.g.__bufadd = 0, 0, 0",
    "btv.on('WinNew', {}, function() vim.g.__winnew = (vim.g.__winnew or 0) + 1 end)",
    "btv.on('TabNew', {}, function() vim.g.__tabnew = (vim.g.__tabnew or 0) + 1 end)",
    "btv.on('BufAdd', {}, function() vim.g.__bufadd = (vim.g.__bufadd or 0) + 1 end)",
  ].join("\n") + "\n");
  await page.reload();
  await boot();
  await buildLayout();
  const windows = await page.evaluate(() => (window.__bemtvi.frame().windows || []).length);
  check("session: the test layout is live before the flush (a 2-window split)", windows === 2, `windows=${windows}`);
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  const withOptIn = await readShada();
  let parsed = null;
  try { parsed = JSON.parse(withOptIn); } catch {}
  check("session: `btv.shada.save_layout(true)` captures the layout into the OPFS blob",
    !!parsed && parsed.session != null,
    `blob=${withOptIn === null ? "MISSING" : `${withOptIn.length} bytes, session=${JSON.stringify(parsed?.session ?? null)?.slice(0, 60)}`}`);
  // The captured session must describe the real layout, not an empty husk: both files
  // present, so a restore has something to rebuild.
  const blobStr = JSON.stringify(parsed?.session ?? {});
  check("session: the captured layout names every open file, across both tabs",
    /a\.txt/.test(blobStr) && /b\.txt/.test(blobStr) && /c\.txt/.test(blobStr),
    `session=${blobStr.slice(0, 240)}`);

  // --- 2. A session in the blob re-loads without error on the next boot. ---
  await page.reload();
  await boot();
  await sleep(600);
  check("session: a blob carrying a layout loads cleanly on the next boot",
    !errors.some((e) => /shada parse|shada load/i.test(e)), errors.join(" | "));

  // --- 2b. Phase 2: the layout is REBUILT — the split comes back on the new boot. ---
  // The window tree is rebuilt synchronously; each restored leaf's text is an async OPFS
  // fetch (`open_buffer_for_restore` enqueues a replica open when the fs is off-tick), so
  // poll for the tree first and the contents after.
  let restored = null;
  for (let i = 0; i < 80; i++) {
    restored = await page.evaluate(() => (window.__bemtvi.frame().windows || [])
      .map((w) => w.file_name || ""));
    if (restored.length === 2) break;
    await sleep(100);
  }
  check("session: the captured split is rebuilt at boot (2 windows)",
    restored && restored.length === 2, `windows=${JSON.stringify(restored)}`);
  check("session: each restored window is on the file it was captured with",
    !!restored && restored.some((n) => /a\.txt$/.test(n)) && restored.some((n) => /b\.txt$/.test(n)),
    `files=${JSON.stringify(restored)}`);
  // The restored buffers' text lands from OPFS a tick or two later; the focused one is
  // what `lines()` reports, and it must be the file's real contents, not an empty husk.
  let text = "";
  for (let i = 0; i < 80; i++) {
    text = await page.evaluate(() => window.__bemtvi.lines());
    if (/alpha|beta/.test(text)) break;
    await sleep(100);
  }
  check("session: a restored window's buffer fills in from OPFS",
    /alpha|beta/.test(text), `lines=${JSON.stringify(String(text).slice(0, 60))}`);
  // …and the CURSOR comes back with it. This is the half an off-tick fs breaks silently:
  // the restore rebuilds the window while the leaf's bytes are still in flight, so the
  // clamp against the empty replica snaps the saved line to the top, and a tick later the
  // text lands and the window looks perfectly restored with the cursor on line 1. The core
  // records the awaited position instead (`note_pending_open_cursor` → `settle_loaded_cursor`).
  // `execLua` renders a non-int result with `Debug`, so read the row out of a tagged
  // string rather than parsing the raw value.
  let row = "";
  for (let i = 0; i < 40; i++) {
    row = String(await luaResult('return "row<" .. tostring(btv.cursor.get(0)[1]) .. ">"'));
    if (/row<3>/.test(row)) break;
    await sleep(100);
  }
  check("session: the restored window's cursor comes back where it was left (line 3)",
    /row<3>/.test(row), `cursor=${JSON.stringify(row)}`);
  const tabs = await luaResult("return tostring(#btv.tabpage.list())");
  check("session: the second tab is rebuilt too", /\b2\b/.test(String(tabs)), `tabs=${JSON.stringify(tabs)}`);

  // --- 2c. The restored layout is STARTUP state, not something that "appeared": the
  //         re-baseline in `boot_finish` must leave the config's WinNew / TabNew / BufAdd
  //         autocmds unfired on a restoring boot. Without it every window, tab and buffer
  //         the restore minted fires one. The counters are per-boot (`vim.g` is not
  //         persisted), and nothing but the restore creates a window on this boot, so a
  //         non-zero count here is exactly the spurious-event bug.
  const fired = await luaResult(
    'return string.format("%s/%s/%s", tostring(vim.g.__winnew or 0), ' +
    'tostring(vim.g.__tabnew or 0), tostring(vim.g.__bufadd or 0))');
  check("session: a restoring boot fires no spurious WinNew / TabNew / BufAdd",
    /\b0\/0\/0\b/.test(String(fired)), `winnew/tabnew/bufadd=${JSON.stringify(fired)}`);

  // --- 2d. Phase 4: the workspace surfaces report the web model honestly. On this build
  //         the ORIGIN is the workspace (one OPFS shada blob per origin), so
  //         `btv.workspace.active()` is true and `dir()` is the session root — they used to
  //         say `false`/`nil`, which is what made a plugin gating persistence on them (e.g.
  //         bemtvi-dap's per-workspace store) quietly skip it in a browser.
  const ws = await luaResult(
    'return tostring(btv.workspace.active()) .. "|" .. tostring(btv.workspace.dir())');
  check("workspace: the web session reports itself as a workspace rooted at the OPFS root",
    /true\|\//.test(String(ws)), `active|dir=${JSON.stringify(ws)}`);

  // --- 2e. Phase 4: a `btv.wso` override survives the reload. `apply_persist` already
  //         applies the overlay on load; only the wasm EXPORT dropped it, so it round-trips
  //         now. Set it, flush, reload, read it back.
  await luaResult('btv.wso.timeoutlen = 733');
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  await page.reload();
  await boot();
  await sleep(800);
  const wso = await luaResult('return tostring(btv.wso.timeoutlen)');
  check("workspace: a btv.wso override persists across a reload",
    /\b733\b/.test(String(wso)), `wso.timeoutlen=${JSON.stringify(wso)}`);

  // --- 2f. The restored-focus HOLD releases on the first user input. A restore stashes
  //         the layer the session was quit from and `settle_events` re-asserts it on EVERY
  //         settle (an fs completion, an LSP reply, a watch) until the user acts — natively
  //         `btv_input` / `btv_input_mouse` release it, and the web build had no release
  //         point at all, so a restored session kept yanking focus back out of wherever you
  //         moved it. Capture with focus on MAIN and a dock open, then move into the dock
  //         with a real keystroke and let an async `btv.fs` op settle: focus must stay put.
  await wipeOpfs();
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    for (const [n, t] of [["a.txt", "alpha\n"], ["b.txt", "beta\n"]]) {
      const fh = await root.getFileHandle(n, { create: true });
      const w = await fh.createWritable(); await w.write(t); await w.close();
    }
  });
  await writeConfig([
    "btv.shada.save_layout(true)",
    "btv.cmd('edit /a.txt')",
    "local v = btv.view.create{ name = 'Side', persist = 'side-1', namespace = 'focusdemo' }",
    "v:set_lines({ 'side' })",
    "v:mount{ dock = 'left', size = 20 }",
    // Mounting a dock focuses it; park focus back on main so "main" is what gets captured.
    "btv.layer.main()",
  ].join("\n") + "\n");
  await page.reload();
  await boot();
  await sleep(900);
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  await writeConfig([
    "btv.shada.save_layout(true)",
    "btv.view.on_restore(function(id, place)",
    "  local nv = btv.view.create{ name = 'Side', persist = id, namespace = 'focusdemo' }",
    "  nv:set_lines({ 'side' })",
    "  place(nv)",
    "end, 'focusdemo')",
  ].join("\n") + "\n");
  await page.reload();
  await boot();
  // The restore lands the captured layer ("main"), so the focused buffer is the file.
  let onMain = "";
  for (let i = 0; i < 80; i++) {
    onMain = String(await luaResult("return tostring(vim.api.nvim_buf_get_name(0))"));
    if (/a\.txt/.test(onMain)) break;
    await sleep(100);
  }
  check("focus: the restore lands on the layer the session was quit from (main)",
    /a\.txt/.test(onMain), `buffer=${JSON.stringify(onMain)}`);
  // A real keystroke moves focus into the dock — and releases the hold.
  await page.evaluate(() => window.__bemtvi.feed(":DockFocus left<CR>"));
  await sleep(400);
  const inDock = String(await luaResult("return tostring(vim.api.nvim_buf_get_name(0))"));
  check("focus: a keystroke moves focus into the dock", !/a\.txt/.test(inDock),
    `buffer=${JSON.stringify(inDock)}`);
  // An async `btv.fs` read settles (`fs_op_result` → `settle_events`), which is where the
  // hold would re-assert. `execLua` is not user input, so nothing else clears it here.
  await luaResult("btv.fs.read('/b.txt')");
  await sleep(600);
  const afterSettle = String(await luaResult("return tostring(vim.api.nvim_buf_get_name(0))"));
  check("focus: an async settle after user input does NOT yank focus back to the restored layer",
    !/a\.txt/.test(afterSettle), `buffer=${JSON.stringify(afterSettle)}`);

  // --- 2g. A stale blob outliving its files. The plan listed this as a risk "worth an
  //         explicit test" and shipped without one. Capture a two-file split, delete one
  //         file from OPFS, reload: the restore must degrade, not break.
  await wipeOpfs();
  await writeConfig("btv.shada.save_layout(true)\n");
  await page.reload();
  await boot();
  await buildLayout();
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    try { await root.removeEntry("b.txt"); } catch {}
  });
  await page.reload();
  await boot();
  await sleep(1500);
  const stale = await page.evaluate(() => ({
    wins: (window.__bemtvi.frame().windows || []).map((w) => w.file_name || ""),
  }));
  check("session: a layout naming a since-deleted file still boots (no error, no hang)",
    !errors.some((e) => /shada parse|shada load/i.test(e)),
    `errors=${errors.join(" | ")} windows=${JSON.stringify(stale.wins)}`);
  // Off-tick fs is the difference from native here: `open_buffer_for_restore` CANNOT know
  // the file is gone (the OPFS read is async), so the leaf is kept and its buffer stays
  // empty — exactly what `:e /gone.txt` gives you on this build. Natively the synchronous
  // read fails and `build_layout` drops the leaf, collapsing the split. Documented as an
  // accepted divergence in docs/plans/2026-08-14-web-session-restore.md rather than
  // silently assumed to behave like native.
  check("session: the missing file's window survives as an empty buffer (the web `:e` contract)",
    stale.wins.length === 2 && stale.wins.some((n) => /b\.txt$/.test(n)),
    `windows=${JSON.stringify(stale.wins)}`);

  // --- 3. Not opted in: no layout is captured (default off, as natively). ---
  await wipeOpfs();
  await writeConfig("vim.g.__no_layout = 1\n");
  await page.reload();
  await boot();
  await buildLayout();
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  const withoutOptIn = await readShada();
  let parsed2 = null;
  try { parsed2 = JSON.parse(withoutOptIn); } catch {}
  check("session: without the opt-in the blob carries no layout",
    !!parsed2 && parsed2.session == null,
    `blob=${withoutOptIn === null ? "MISSING" : `${withoutOptIn.length} bytes, session=${JSON.stringify(parsed2?.session ?? null)?.slice(0, 60)}`}`);

  if (errors.length) console.log("  console/page output:\n   " + errors.join("\n   "));
  await wipeOpfs();
  await browser.close();
} finally { cleanup(); }

console.log(failures === 0
  ? "\nALL PASS — the web build captures its window/tab layout into shada when a config opts in, and rebuilds it at the next boot"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
