// Repro/regression for the bug "bemtvi-diff gutter signs don't show on the web build":
// a window that reserves a sign column (`signcolumn=yes`) but has NO line numbers
// (`nonumber`) — like an `btv.view` diff pane — dropped its signs on wasm. The redraw
// frame carries them fine (`sign_width`/`diagnostics_signs` project on every build);
// the CLIENT bug was `renderLine` gating the whole gutter on `gutterW` (the number
// width) alone, so with `number_width === 0` the sign cell was never rendered. Every
// sibling render path already checked `sign_width`; this asserts `renderLine` does too,
// by reading the actual rendered gutter DOM.
//
//   node verify-sign-gutter-no-number.mjs   (serves web/ live; uses the built dist/eh.mjs)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8097;

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

  // A buffer with text, NO line numbers, a reserved sign column, and one `sign_text`
  // extmark on line 1 — the exact shape an btv.view diff pane paints (signs, no numbers).
  await page.evaluate(() => window.__bemtvi.feed("ggdGialpha<Esc>"));
  await luaResult(`
    vim.wo.number = false
    vim.wo.relativenumber = false
    vim.wo.signcolumn = "yes"
    btv.hl.define(0, "MySignHl", { fg = "#89b4fa", bold = true })
    local ns = btv.ns.create("sign-gutter-test")
    btv.buf.set_extmark(0, ns, 0, 0, { sign_text = "▶", sign_hl_group = "MySignHl" })
    return true
  `);
  // Nudge a fresh frame and let it render.
  await page.evaluate(() => window.__bemtvi.feed("0"));
  await sleep(250);

  // (a) The redraw frame carries the sign column + glyph (projection sanity — this part
  // already worked on web; if it fails the bug is server-side, not the renderer).
  const frame = await page.evaluate(() => {
    const w = (window.__bemtvi.frame()?.windows || []).find((x) => x.focused) || {};
    const signs = w.diagnostics_signs || [];
    const glyphRow = signs.find((s) => Array.isArray(s) && typeof s[0] === "string");
    return { number_width: w.number_width || 0, sign_width: w.sign_width || 0, glyph: glyphRow ? glyphRow[0] : null };
  });
  check("frame reserves a sign column with no line numbers",
    frame.sign_width > 0 && frame.number_width === 0, `frame=${JSON.stringify(frame)}`);
  check("frame carries the ▶ sign glyph", frame.glyph === "▶", `frame=${JSON.stringify(frame)}`);

  // (b) THE BUG: the rendered gutter DOM actually paints the ▶ glyph. Before the fix the
  // gutter was skipped entirely (number_width === 0 → no renderGutterCell call), so no
  // row contained the glyph.
  const painted = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row")].some((r) => r.textContent.includes("▶")));
  check("the sign glyph ▶ is painted in the gutter DOM", painted,
    "no rendered row contained the sign glyph — renderLine dropped the gutter");

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — sign column renders without line numbers on the web build"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
