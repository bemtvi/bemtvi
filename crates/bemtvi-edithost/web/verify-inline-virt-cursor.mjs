// The Insert cursor beside inline virtual text, in the real wasm edit-host and DOM
// renderer. A character typed at a right-gravity mark's anchor lands before the mark's
// text, so the Insert bar sits there too (on the text's first cell), not on the glyph
// after it; left-gravity text stays before the bar, and in Normal mode the block is on
// the real character. Also at the end of a line.
//
//   node verify-inline-virt-cursor.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8198;

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
process.on("exit", () => { try { srv.kill(); } catch {} });

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => { console.log("  [pageerror]", e.message); failures++; });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  const lua = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);
  const feed = async (keys) => { await page.evaluate((k) => window.__bemtvi.feed(k), keys); await sleep(150); };
  // The cursor element: its text, its class, and the painted row around it.
  const cursor = () => page.evaluate(() => {
    const el = document.getElementById("btv-cursor");
    if (!el) return null;
    let row = el.parentElement;
    while (row && !/ab/.test(row.textContent)) row = row.parentElement;
    // The row's text up to the cursor element: where it sits in what's painted.
    let before = "";
    if (row) {
      const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
      for (let n = walker.nextNode(); n; n = walker.nextNode()) {
        if (el.contains(n)) break;
        before += n.textContent;
      }
    }
    return { text: el.textContent, cls: el.className, before: before.trimStart() };
  });

  await feed(":set nonumber norelativenumber<CR>ggdGiab|cd<Esc>");
  await lua(`local ns = btv.ns.create("cursorvt")
    vim.api.nvim_buf_set_extmark(0, ns, 0, 2, {
      virt_text = { { "RRR", "Comment" } }, virt_text_pos = "inline",
    })
    vim.api.nvim_buf_set_extmark(0, ns, 0, 2, {
      virt_text = { { "LL", "Comment" } }, virt_text_pos = "inline", right_gravity = false,
    })`);
  await sleep(150);

  // `a` on `b`: the bar is on RRR's first cell, after LL.
  await feed("0la");
  let c = await cursor();
  check("Insert: the bar sits after the left-gravity text, on the right-gravity text's first cell",
    c && c.text === "R" && c.cls === "cur-bar" && c.before.endsWith("abLL"), JSON.stringify(c));

  // Typing lands there, and the bar stays in front of RRR.
  await feed("x");
  const line = await lua("return btv.buf.lines(0, 0, 1)[1]");
  check("the typed character went before the marks' text", String(line).includes("abx|cd"), line);
  c = await cursor();
  // LL keeps its place (left gravity) and RRR moved on with the `|`.
  check("after typing the bar is still before RRR, with LL and the x behind it",
    c && c.text === "R" && c.before.endsWith("abLLx"), JSON.stringify(c));

  // Normal mode on the `|`: the block is on the real character, past both.
  await feed("<Esc>l");
  c = await cursor();
  check("Normal: the block is on the real character", c && c.text === "|" && c.cls === "cur-block",
    JSON.stringify(c));

  // End of line: a right-gravity mark past the last character, `A` appends before it.
  await lua(`local ns = btv.ns.create("cursorvt-eol")
    vim.api.nvim_buf_set_extmark(0, ns, 0, 6, {
      virt_text = { { "EEE", "Comment" } }, virt_text_pos = "inline",
    })`);
  await feed("A");
  c = await cursor();
  check("Insert at the end of the line: the bar is on the trailing text's first cell",
    c && c.text === "E" && c.cls === "cur-bar" && c.before.endsWith("cd"), JSON.stringify(c));

  await browser.close();
} catch (e) {
  console.log("FAIL ", e.message);
  failures++;
}
console.log(failures ? `${failures} failure(s)` : "all checks passed");
process.exit(failures ? 1 : 0);
