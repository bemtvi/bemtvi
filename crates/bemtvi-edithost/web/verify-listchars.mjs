// Focused verifier for `'list'` / `'listchars'` in the web client: with `list` on,
// a tab and trailing blanks must paint as their glyphs, colored apart from the real
// text (the server's `Whitespace` overlay), and `nolist` restores the plain row.
// Drives the real wasm edit-host in headless Chromium.
//
//   node verify-listchars.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8117;

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

// The first text row of the focused window, as one {ch, color} entry per painted
// character (gutter cells dropped).
const firstRow = () => {
  const row = document.querySelector("#grid .win .row");
  const out = [];
  const walk = (node, color) => {
    if (node.nodeType === Node.TEXT_NODE) {
      for (const ch of node.textContent) out.push({ ch, color });
      return;
    }
    if (node.classList?.contains("gutter")) return;
    const c = node.nodeType === Node.ELEMENT_NODE ? getComputedStyle(node).color : color;
    for (const child of node.childNodes) walk(child, c);
  };
  walk(row, "");
  return out;
};

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  await page.evaluate(() =>
    window.__bemtvi.execLua(
      "btv.buf.set_lines(0, 0, -1, false, { 'x\\tb  ' }); vim.bo.tabstop = 4; " +
        "vim.wo.listchars = 'tab:>-,trail:~'; vim.wo.list = true",
    ),
  );
  await sleep(300);
  const on = await page.evaluate(firstRow);
  const text = on.map((c) => c.ch).join("").trim();
  check("the tab and trailing blanks paint as glyphs", text.endsWith("x>--b~~"), JSON.stringify(text));
  const at = (s) => on.filter((c) => c.ch === s).map((c) => c.color);
  const x = at("x")[0];
  const glyphs = [...at(">"), ...at("-"), ...at("~")];
  check(
    "every glyph is colored apart from the text",
    glyphs.length === 5 && glyphs.every((c) => c && c !== x),
    JSON.stringify({ x, glyphs }),
  );

  await page.evaluate(() => window.__bemtvi.execLua("vim.wo.list = false"));
  await sleep(300);
  const off = (await page.evaluate(firstRow)).map((c) => c.ch).join("");
  check("nolist draws the plain row", !/[>~]/.test(off) && off.includes("x"), JSON.stringify(off));

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
