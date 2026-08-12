// Repro harness for the decor-todo bug: source the btv.decor TODO-keyword config from
// OPFS, type a buffer with keywords, and check (a) the provider ran + published marks
// Lua-side, (b) the extmarks landed in core, and (c) the highlight spans reached the
// redraw frame the renderer paints. The bug: (a)+(b) hold but (c) is empty on web.
//
//   node verify-decor.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8094;

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

const INIT_LUA = `
local KEYWORDS = { TODO = "TodoKeyword", FIXME = "FixmeKeyword" }
btv.hl.define(0, "TodoKeyword", { fg = "#89b4fa", bold = true })
btv.hl.define(0, "FixmeKeyword", { fg = "#f38ba8", bold = true })
btv.decor.provider({
  name = "todo-keywords",
  debounce = 60,
  on_range = function(ctx, publish)
    local marks = {}
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for word, group in pairs(KEYWORDS) do
        local from = 1
        while true do
          local s, e = line:find(word, from, true)
          if not s then break end
          marks[#marks + 1] = { row, s - 1, end_col = e, hl = group }
          from = e + 1
        end
      end
    end
    publish(marks)
  end,
})
vim.o.number = true
`;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  page.on("console", (m) => { if (m.type() === "error") console.log("  [console.error]", m.text()); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await writeOpfs(page, "init.lua", INIT_LUA);
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  const luaResult = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

  // Type a buffer with two keywords on line 1.
  await page.evaluate(() => window.__bemtvi.feed("ggdGiTODO and FIXME here<Esc>"));
  // Wait past the 60ms debounce + a redraw.
  await sleep(400);
  // Nudge so a fresh frame is posted after the debounce fired.
  await page.evaluate(() => window.__bemtvi.feed("0"));
  await sleep(200);

  // (a) Lua-side: the provider ran and published marks.
  const last = await luaResult("local l = btv._decor.last; return l and (l.name .. ':' .. #l.marks) or 'nil'");
  check("decor: provider ran + published marks (Lua btv._decor.last)",
    /todo-keywords:[1-9]/.test(String(last)), `last=${JSON.stringify(last)}`);

  // (b) core extmarks landed in the provider namespace.
  const exts = await luaResult(`
    local ns = btv.ns.create("btv.decor:todo-keywords")
    local m = vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, {})
    return #m
  `);
  check("decor: extmarks present in core for the provider ns",
    Number((String(exts).match(/(\d+)/) || [])[1]) >= 1, `count=${JSON.stringify(exts)}`);

  // (c) render-side: the highlight spans reached the redraw frame the renderer paints.
  const hlInfo = await page.evaluate(() => {
    const w = (window.__bemtvi.frame()?.windows || []).find((x) => x.focused) || {};
    const hls = w.highlights || [];
    const total = hls.reduce((n, row) => n + ((row && row.length) || 0), 0);
    return { rows: hls.length, total, row0: JSON.stringify(hls[0] || null) };
  });
  check("decor: highlight spans reached the redraw frame (window.highlights)",
    hlInfo.total >= 1, `frame highlights=${JSON.stringify(hlInfo)}`);

  // (d) the DOM actually paints the keyword colors (TODO=#89b4fa, FIXME=#f38ba8). Probe
  // every rendered span's inline color so we confirm the overlay reached the screen.
  const colors = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")]
      .map((s) => (s.getAttribute("style").match(/color:\s*([^;]+)/) || [])[1])
      .filter(Boolean)
      .map((c) => c.trim().toLowerCase()));
  check("decor: TODO keyword painted blue (#89b4fa) in the DOM",
    colors.includes("#89b4fa"), `colors=${JSON.stringify(colors)}`);
  check("decor: FIXME keyword painted red (#f38ba8) in the DOM",
    colors.includes("#f38ba8"), `colors=${JSON.stringify(colors)}`);

  await page.evaluate(async () => {
    try { (await navigator.storage.getDirectory()).removeEntry("init.lua"); } catch {}
  });
  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — btv.decor renders on the web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
