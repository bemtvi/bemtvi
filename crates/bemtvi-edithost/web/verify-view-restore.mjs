// Playwright verifier for persisted `btv.view` slots on the web build (Phase 3 of
// docs/plans/2026-08-14-web-session-restore.md).
//
// A view created with `persist = <id>` occupies a window in the captured layout. On the
// next boot the restore reserves that window as a placeholder and hands the slot to the
// owning plugin's `btv.view.on_restore` handler, which rebuilds the content and `place`s
// it. A slot whose namespace registers no handler must COLLAPSE, not linger as an empty
// placeholder window.
//
//   node verify-view-restore.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8131;

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

// The view is a plugin-owned surface, so it declares its namespace explicitly — the
// escape hatch `btv.view.create` / `on_restore` document for a no-attribution context
// (here, a bare init.lua rather than a plugin under a runtimepath).
const CAPTURE_CFG = [
  "btv.shada.save_layout(true)",
  "btv.cmd('edit /a.txt')",
  "local v = btv.view.create{ name = 'Notes', persist = 'notes-1', namespace = 'demo' }",
  "v:set_lines({ 'note one' })",
  "v:mount{ split = 'split' }",
].join("\n") + "\n";

// Same session, next boot: the plugin is "loaded" (its handler registered from the
// config) and adopts the slot the restore reserved for it.
const RESTORE_CFG = [
  "btv.shada.save_layout(true)",
  "vim.g.__restored = 'none'",
  "btv.view.on_restore(function(id, place)",
  "  local nv = btv.view.create{ name = 'Notes', persist = id, namespace = 'demo' }",
  "  nv:set_lines({ 'restored:' .. id })",
  "  place(nv)",
  "  vim.g.__restored = id",
  "end, 'demo')",
].join("\n") + "\n";

// The orphan case: nothing registers for `demo`, so its reserved slot must collapse.
const ORPHAN_CFG = "btv.shada.save_layout(true)\nvim.g.__restored = 'none'\n";

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
  page.on("console", (m) => { if (/config_error|E5117/i.test(m.text())) errors.push(m.text()); });

  const boot = async () => {
    await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 30000 });
    await page.evaluate(() => window.__bemtvi.ready);
  };
  const writeFile = (name, text) => page.evaluate(async ({ n, t }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(n, { create: true });
    const w = await fh.createWritable(); await w.write(t); await w.close();
  }, { n: name, t: text });
  const wipeOpfs = () => page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    for await (const [n] of root.entries()) { try { await root.removeEntry(n, { recursive: true }); } catch {} }
  });
  const luaResult = (code) =>
    page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);
  const winCount = () => page.evaluate(() => (window.__bemtvi.frame().windows || []).length);

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await boot();

  // ---- Capture: a file window plus a persisted view window. ----
  await wipeOpfs();
  await writeFile("a.txt", "alpha\n");
  await writeFile("init.lua", CAPTURE_CFG);
  await page.reload();
  await boot();
  await sleep(900);
  const captured = await winCount();
  check("view: the capture boot has a file window + the mounted view window", captured === 2,
    `windows=${captured}`);
  await page.evaluate(() => window.__bemtvi.shadaFlush());
  await sleep(1200);
  const blob = await page.evaluate(async () => {
    try {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(".bemtvi");
      return await (await (await dir.getFileHandle("shada")).getFile()).text();
    } catch { return null; }
  });
  check("view: the persisted view's slot is recorded in the captured layout",
    !!blob && /notes-1/.test(blob) && /demo/.test(blob),
    `blob=${blob === null ? "MISSING" : blob.length + " bytes"}`);

  // ---- Restore: the owning namespace's handler adopts the reserved slot. ----
  await writeFile("init.lua", RESTORE_CFG);
  await page.reload();
  await boot();
  let restored = "";
  for (let i = 0; i < 80; i++) {
    restored = String(await luaResult("return tostring(vim.g.__restored)"));
    if (/notes-1/.test(restored)) break;
    await sleep(100);
  }
  check("view: on_restore is handed the persisted id and adopts its slot",
    /notes-1/.test(restored), `vim.g.__restored=${JSON.stringify(restored)}`);
  const afterRestore = await winCount();
  check("view: the restored view keeps its window (file + view)", afterRestore === 2,
    `windows=${afterRestore}`);
  const painted = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row")].map((r) => r.textContent).join("\n"));
  check("view: the rebuilt content is painted in the adopted window",
    /restored:notes-1/.test(painted), `screen=${JSON.stringify(painted.slice(0, 120))}`);

  // ---- Orphan: no handler for `demo`, so the reserved slot collapses. ----
  await writeFile("init.lua", ORPHAN_CFG);
  await page.reload();
  await boot();
  await sleep(1500);
  const orphan = await winCount();
  check("view: an unclaimed slot collapses instead of leaving a placeholder window",
    orphan === 1, `windows=${orphan}`);

  if (errors.length) console.log("  console/page output:\n   " + errors.join("\n   "));
  await wipeOpfs();
  await browser.close();
} finally { cleanup(); }

console.log(failures === 0
  ? "\nALL PASS — a persisted btv.view slot is reserved, handed back to its plugin, and collapsed when unclaimed"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
