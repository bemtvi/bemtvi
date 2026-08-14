// Reproduce the real bemtvi-diff scenario on web: an `btv.view` pane that reserves a sign
// column and paints a `sign_text` extmark via `:set_decor`, with NO line numbers — and
// check the sign reaches BOTH the redraw frame and the rendered gutter DOM. (My earlier
// verify-sign-gutter-no-number used a plain buffer; the diff panes are btv.view buffers,
// so this exercises the view set_decor → extmark → merged_sign_cells path end to end.)
//
//   node verify-view-sign.mjs   (serves web/ live; uses the built dist/eh.mjs)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8098;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const lin = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  if (lin.length) return lin[lin.length - 1];
  const mac = globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`).sort();
  return mac.length ? mac[mac.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

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

  const luaResult = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

  await page.evaluate(() => window.__bemtvi.feed("imain<Esc>"));
  // Create + mount a named view, reserve its sign column, paint a sign_text decor on
  // line 1 — exactly what a diff pane does.
  await luaResult(`
    btv.hl.define(0, "MySignHl", { fg = "#89b4fa", bold = true })
    vw = btv.view.create{ name = "ours" }
    vw:set_lines{ "alpha", "beta" }
    vw:mount{ split = "vsplit" }
    return true
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  // The bufnr/winid mirror settles a tick after mount; now set signcolumn + the decor.
  await luaResult(`
    local win = vw:winid()
    if win then vim.wo[win].signcolumn = "yes"; vim.wo[win].number = false end
    ns = btv.ns.create("view-sign-test")
    vw:set_decor(ns, { { line = 0, col = 0, sign_text = "▶", sign_hl_group = "MySignHl" } })
    return win and "win:" .. tostring(win) or "no-win"
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(250);

  // (a) The view buffer actually carries the sign extmark in core.
  const exts = await luaResult(`return #vim.api.nvim_buf_get_extmarks(vw:bufnr(), ns, 0, -1, {})`);
  // `execLua` renders its result as `ok:<value>`, so `Number("ok:1")` is NaN and the bare
  // numeric compare could never pass. Strip the prefix before comparing, as the other
  // verifiers do by matching on content.
  const extCount = Number(String(exts).replace(/^ok:/, ""));
  check("the view buffer carries the sign extmark", extCount >= 1, `count=${JSON.stringify(exts)}`);

  // (b) The redraw frame for the view window carries sign_width + the glyph.
  const frame = await page.evaluate(() => {
    const wins = window.__bemtvi.frame()?.windows || [];
    // The view window shows "ours" / its content; find the one whose lines include alpha.
    const w = wins.find((x) => (x.lines || []).includes("alpha")) || {};
    const signs = w.diagnostics_signs || [];
    const glyphRow = signs.find((s) => Array.isArray(s) && typeof s[0] === "string");
    return { found: !!w.lines, number_width: w.number_width || 0, sign_width: w.sign_width || 0, glyph: glyphRow ? glyphRow[0] : null };
  });
  check("the view window's redraw carries a sign column", frame.found && frame.sign_width > 0, `frame=${JSON.stringify(frame)}`);
  check("the view window's redraw carries the ▶ glyph", frame.glyph === "▶", `frame=${JSON.stringify(frame)}`);

  // (c) The rendered gutter DOM paints the ▶ glyph.
  const painted = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row")].some((r) => r.textContent.includes("▶")));
  check("the sign glyph ▶ is painted in the gutter DOM", painted, "no rendered row contained the sign glyph");

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — an btv.view sign decor renders in the gutter on web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
