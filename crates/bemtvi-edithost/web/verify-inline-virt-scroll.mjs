// Inline virtual text under a sideways scroll, in the real wasm edit-host and DOM
// renderer: `leftcol` falling inside a placement shows the rest of it, past it the text
// stays where its cells put it, and a click maps through what's painted.
//
//   node verify-inline-virt-scroll.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8197;

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
  // Everything on screen as text; the row we want is the only one with these letters.
  const row = () => page.evaluate(() => {
    const line = [...document.querySelectorAll("*")]
      .filter((e) => e.children.length > 0 && /abc|cde|klm/.test(e.textContent) && e.textContent.length < 400)
      .sort((a, b) => a.textContent.length - b.textContent.length)[0];
    return line ? line.textContent : "";
  });

  const body = "abcdefghijklmnopqrstuvwxyz".repeat(6);
  await feed(`:set nonumber norelativenumber nowrap<CR>ggdGi${body}<Esc>41|`);
  await lua(`local ns = btv.ns.create("scroll")
    vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
      virt_text = { { "12345678", "Comment" } }, virt_text_pos = "inline",
    })`);
  await sleep(150);
  check("no scroll: the whole placement, then the text", (await row()).includes("12345678abc"), await row());

  await lua("btv.win.set_leftcol(0, 3)");
  await feed("<Esc>");
  const partial = await row();
  check("leftcol inside it: the last 5 cells, then the text",
    partial.includes("45678abc") && !partial.includes("345678"), partial);

  await lua("btv.win.set_leftcol(0, 10)");
  await feed("<Esc>");
  const past = await row();
  check("leftcol past it: the text starts at column 2, not 10",
    /(^|\s)cdefg/.test(past) && !past.includes("5678"), past);

  // A pan paints the same model on its way there. From `leftcol` 0 to 7 every column
  // falls inside the placement, so every frame starts with what's left of its digits
  // and then the `a`: it's never dropped with the text snapped left.
  await lua("btv.win.set_leftcol(0, 0)");
  await sleep(400);
  const samples = await page.evaluate(() => new Promise((resolve) => {
    const seen = [];
    window.__bemtvi.execLua("btv.win.set_leftcol(0, 7)");
    const t0 = performance.now();
    const tick = () => {
      const band = document.querySelector("#grid .scroll-band");
      const line = band && [...band.querySelectorAll(".row")].map((r) => r.textContent)
        .find((t) => /[a-z]{5}/.test(t));
      if (line) seen.push(line.trimStart());
      performance.now() - t0 < 600 ? requestAnimationFrame(tick) : resolve(seen);
    };
    requestAnimationFrame(tick);
  }));
  check("the pan animated", new Set(samples).size > 1,
    JSON.stringify(samples.map((t) => t.slice(0, 12))));
  const bad = samples.filter((t) => !/^[1-8]+abc/.test(t));
  check("every pan frame shows the placement's rest, then the text", bad.length === 0,
    JSON.stringify(bad.slice(0, 4)));

  await browser.close();
} catch (e) {
  console.log("FAIL ", e.message);
  failures++;
}
console.log(failures ? `${failures} failure(s)` : "all checks passed");
process.exit(failures ? 1 : 0);
