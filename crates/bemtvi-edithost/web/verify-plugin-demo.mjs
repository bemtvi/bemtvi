// Playwright verifier for the python demo's first-party plugin set + catppuccin + demo
// init.lua (Phase 6 of docs/plans/2026-06-23-web-python-demo.md). Runs against the assembled
// **demo** site (build-demo.sh → demo-site/), where build-config flips `plugins: true` so the
// Worker fetches + sources the vendored amalgamated bundle (build-plugins.sh →
// web/vendor/plugins/plugins-bundle.lua) at boot. We seed the committed web/demo-init.lua into
// OPFS as /init.lua (Phase 7 will auto-seed it) and assert, after a fresh boot:
//
//   - every first-party module (catppuccin + the five bemtvi-* plugins) loaded from the bundle
//     (package.loaded) — proving the amalgamated require()s resolved and init.lua ran them;
//   - catppuccin (mocha) applied — the Normal highlight carries the mocha palette
//     (fg #cdd6f4 / bg #1e1e2e), read back from the real highlight registry;
//   - the statusline RENDERS — bemtvi-line projected styled status segments into the redraw
//     frame, including the "NORMAL" mode label (its lualine_a mode component);
//   - the python LSP is configured + enabled — btv.lsp has basedpyright with its cmd.
//
// Prereqs: ./build-demo.sh (assembles demo-site/) and a Chromium for Playwright. Run:
//   node verify-plugin-demo.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, readFileSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8123;
const DEMO_SITE = `${here}../demo-site`;
const DEMO_INIT = readFileSync(`${here}demo-seed/init.lua`, "utf8");

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

async function writeOpfs(page, name, text) {
  await page.evaluate(async ({ name, text }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(name, { create: true });
    const w = await fh.createWritable();
    await w.write(text);
    await w.close();
  }, { name, text });
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, BEMTVI_SERVE_ROOT: DEMO_SITE },
});
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  const cfgErrors = [];
  page.on("console", (m) => { const t = m.text(); if (/plugin bundle|config_error|init\.lua/i.test(t)) cfgErrors.push(t); });

  // First load to get an OPFS scope, then seed the demo init.lua.
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await writeOpfs(page, "init.lua", DEMO_INIT);

  // Reload: the Worker fetches + sources the vendored bundle, then sources /init.lua.
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__bemtvi.ready);

  const unwrap = (s) => {
    const m = String(s).match(/Ok\((".*")\)\s*}\s*\)\s*$/s);
    if (m) { try { return JSON.parse(m[1]); } catch { return m[1]; } }
    return String(s);
  };
  const luaResult = (code) =>
    page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code).then(unwrap);

  // 1. Every first-party module loaded from the bundle (package.loaded set by the require()s).
  const PLUGINS = ["catppuccin", "bemtvi-keys-helper", "bemtvi-tree", "bemtvi-line", "bemtvi-lspconfig", "bemtvi-diff",
    "bemtvi-snippets", "bemtvi-markdown-preview"];
  const loaded = await luaResult(
    `local r = {} for _, n in ipairs({${PLUGINS.map((p) => `"${p}"`).join(",")}}) do ` +
    `r[#r+1] = n .. "=" .. tostring(package.loaded[n] ~= nil) end return table.concat(r, " ")`);
  check("demo: all first-party plugins loaded from the bundle",
    PLUGINS.every((p) => loaded.includes(`${p}=true`)), `loaded=${loaded}`);

  // 2. catppuccin mocha applied — Normal highlight carries the mocha palette.
  const normal = await luaResult(
    'local h = vim.api.nvim_get_hl(0, { name = "Normal" }); ' +
    'return string.format("%06x/%06x", h.fg or 0, h.bg or 0)');
  check("demo: catppuccin (mocha) applied — Normal = #cdd6f4 / #1e1e2e",
    /cdd6f4\/1e1e2e/.test(normal), `Normal fg/bg=${normal}`);

  // 3. The statusline renders — bemtvi-line projected styled status segments (incl. the NORMAL
  //    mode label) into the redraw frame the client paints.
  const statusText = await page.evaluate(() => {
    const f = window.__bemtvi.frame();
    const segs = [];
    for (const w of f?.windows || []) for (const s of w.status || []) segs.push(s.text || "");
    return segs.join("");
  });
  check("demo: bemtvi-line statusline renders (NORMAL mode segment in the frame)",
    /NORMAL/.test(statusText), `status=${JSON.stringify(statusText)}`);

  // 4. The python LSP is configured + enabled (basedpyright with its cmd).
  const lsp = await luaResult(
    'return tostring(btv.lsp._enabled["basedpyright"]) .. "|" .. ' +
    'tostring((btv.lsp._config["basedpyright"] or {}).cmd and btv.lsp._config["basedpyright"].cmd[1])');
  check("demo: python LSP configured + enabled (basedpyright)",
    /^true\|basedpyright-langserver/.test(lsp), `lsp=${lsp}`);

  // 5. bemtvi-snippets EXPANDS, rather than merely being registered as a source. Typing the
  //    trigger and accepting the row must splice the snippet body in and land the caret on the
  //    first tabstop — a source that offers nothing (no collection ships with the demo, so the
  //    config registers its own) would still "load" and pass a package.loaded check.
  await page.evaluate(() => window.__bemtvi.feed(":e /snip.py<CR>"));
  await sleep(500);
  await page.evaluate(() => window.__bemtvi.feed("ggdGitry"));
  await sleep(1500);                                     // let the popup gather its sources
  // Walk to the row the SNIPPET source contributed, by kind. Two things make a blind <C-y>
  // useless here: the popup pre-highlights row 0 but leaves it INACTIVE (`selected_active:
  // false`) so the first <C-n> only activates it, and basedpyright also offers `try` as a
  // Keyword — accepting that row inserts the same three characters the trigger already spells,
  // so the buffer looks untouched and a broken snippet source is indistinguishable from a
  // working one. Selecting on `kinds` is what makes this assert the snippet specifically.
  const snipIdx = await page.evaluate(() =>
    ((window.__bemtvi.frame() || {}).menu || { kinds: [] }).kinds.indexOf("Snippet"));
  check("demo: the snippet source contributes a row to the popup", snipIdx >= 0,
    await page.evaluate(() => JSON.stringify((window.__bemtvi.frame() || {}).menu || null)));
  for (let i = 0; i <= snipIdx; i++) {                  // +1: the first <C-n> activates row 0
    await page.evaluate(() => window.__bemtvi.feed("<C-n>"));
    await sleep(150);
  }
  const sel = await page.evaluate(() => {
    const m = (window.__bemtvi.frame() || {}).menu || {};
    return { selected: m.selected, active: m.selected_active, kind: (m.kinds || [])[m.selected] };
  });
  check("demo: the snippet row is the active selection before accept",
    sel.kind === "Snippet" && sel.active === true, JSON.stringify(sel));
  await page.evaluate(() => window.__bemtvi.feed("<C-y>"));   // accept the selected row
  await sleep(600);
  const snipLines = await page.evaluate(() => window.__bemtvi.lines());
  check("demo: bemtvi-snippets expands a snippet (try/except body spliced in)",
    /^try:/m.test(String(snipLines)) && /except Exception as e:/.test(String(snipLines)),
    `lines=${JSON.stringify(snipLines)}`);
  // The tabstop session is live and the jump key moves through it (the config remapped
  // jump_next off <C-j>'s default partner <C-k>, which stays signature help here).
  const tabstop = await luaResult('return tostring(require("bemtvi-snippets").active())');
  check("demo: the expansion opened a live tabstop session", /true/.test(tabstop), `active=${tabstop}`);
  // Leave the session before anything else runs. A live expansion parks the caret on a
  // tabstop in insert (or select, on a placeholder with a default) mode, so a single <Esc>
  // is not enough to guarantee normal mode — and every following `:` command would be typed
  // into the buffer as literal text instead of being executed.
  await luaResult('require("bemtvi-snippets").abort() return 1');
  await page.evaluate(() => window.__bemtvi.feed("<Esc><Esc>"));
  await sleep(300);
  const mode = await page.evaluate(() => window.__bemtvi.execLua('return vim.api.nvim_get_mode().mode').then((r) => r.result));
  check("demo: the editor is back in normal mode after the snippet session",
    /\bn\b/.test(String(mode)), `mode=${mode}`);

  // 6. bemtvi-markdown-preview SERVES. `:MarkdownPreview` mounts the route; fetching it as an
  //    ordinary URL exercises the whole web leg (page fetch -> Service Worker -> edit-host ->
  //    the plugin's Lua on_request), which is the only reason this plugin works in a tab at all.
  await page.evaluate(() => window.__bemtvi.feed(":e /TOUR.md<CR>"));
  await sleep(500);
  await page.evaluate(() => window.__bemtvi.feed(":MarkdownPreview<CR>"));
  await sleep(1500);
  const mount = await page.evaluate(async () => {
    try {
      const r = await fetch("/plugin/bemtvi-markdown-preview/", { cache: "no-store" });
      return { status: r.status, body: (await r.text()).slice(0, 400) };
    } catch (e) { return { status: -1, body: String(e) }; }
  });
  check("demo: bemtvi-markdown-preview serves its page over the Service Worker mount",
    mount.status === 200 && /<html|<!doctype/i.test(mount.body), JSON.stringify(mount));

  // 7. The editor is live and edits (the whole config + bundle didn't brick boot).
  await page.evaluate(() => window.__bemtvi.feed(":e /edit.py<CR>"));
  await sleep(400);
  await page.evaluate(() => window.__bemtvi.feed("ggdGiplugins-ok<Esc>"));
  const edited = await page.evaluate(() => window.__bemtvi.lines());
  check("demo: editor boots + edits with the full plugin config", edited === "plugins-ok", `lines=${JSON.stringify(edited)}`);

  if (cfgErrors.length) console.log("  config_error console output:\n   " + cfgErrors.join("\n   "));

  await page.evaluate(async () => {
    try { (await navigator.storage.getDirectory()).removeEntry("init.lua"); } catch {}
  });
  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — first-party plugin set + catppuccin amalgamated, sourced in the demo build, demo init.lua loads all plugins, theme applies, statusline renders, python LSP configured"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
