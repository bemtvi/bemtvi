// Focused verifier for content-float (nx.ui.float) border + title rendering in the
// web client: the border style must read (rounded corners look rounded) and the title
// must ride the top edge. Drives the real wasm edit-host in headless Chromium.
//
//   node verify-float.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8098;

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
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Open a rounded-border, titled content float.
  await page.evaluate(() => window.__nxvim.execLua(
    "nx.ui.float('hello float\\nsecond line', { title = 'info', border = 'rounded' })"));
  await sleep(150);

  const dom = await page.evaluate(() => {
    // The border lives in the shared .popup-chrome box; content sits in .pmenu inside it.
    const box = document.querySelector("#grid .popup-chrome");
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")].map((r) => r.textContent);
    const titleEl = box.querySelector(".float-title");
    const borderEls = [...box.querySelectorAll(".float-border")].map((e) => e.textContent);
    return { rows, title: titleEl ? titleEl.textContent : null, borderEls };
  });
  check("content float: paints a box", dom !== null, JSON.stringify(dom));

  if (dom) {
    const top = dom.rows[0] || "";
    const bottom = dom.rows[dom.rows.length - 1] || "";
    check("content float: rounded top corners (╭ … ╮)",
      top.startsWith("╭") && top.endsWith("╮"), JSON.stringify(top));
    check("content float: rounded bottom corners (╰ … ╯)",
      bottom.startsWith("╰") && bottom.endsWith("╯"), JSON.stringify(bottom));
    check("content float: title drawn on the top edge, in its own .float-title span",
      dom.title !== null && /info/.test(dom.title), JSON.stringify(dom.title));
    check("content float: title sits right after the corner (left-aligned)",
      top.startsWith("╭ info "), JSON.stringify(top));
    check("content float: side rails use the vertical glyph (│)",
      dom.rows.slice(1, -1).every((r) => r.startsWith("│") && r.endsWith("│")),
      JSON.stringify(dom.rows));
    check("content float: horizontal fill uses ─ (not blank/CSS border)",
      top.includes("─") && bottom.includes("─"), JSON.stringify({ top, bottom }));
  }

  // Now a double-border float to confirm the style switch, not a hardcoded set.
  await page.evaluate(() => window.__nxvim.execLua(
    "nx.ui.float('a wide enough body line', { title = 'd', border = 'double' })"));
  await sleep(150);
  const dbl = await page.evaluate(() => {
    const box = document.querySelector("#grid .popup-chrome");
    const rows = box ? [...box.querySelectorAll(".row")].map((r) => r.textContent) : [];
    return rows[0] || "";
  });
  check("content float: double border uses ═/╔ glyphs",
    dbl.startsWith("╔") && dbl.endsWith("╗") && dbl.includes("═"), JSON.stringify(dbl));

  // ---- Window floats (a real buffer in a float, e.g. nx.view dialogs) ----
  // These render via renderFloat → an opaque chrome box + glyph border + inset
  // content, NOT the nx.ui.float content-float path. The web client used to draw no
  // border, leak the tiled window through a transparent bg, and paint a spurious
  // statusline (the border rows mis-counted as status rows).
  await page.evaluate(() => window.__nxvim.execLua(
    "nx.view.component({\n" +
    "  setup = function() return {} end,\n" +
    "  render = function() return { lines = { 'alpha', 'beta', 'gamma', 'delta' } } end,\n" +
    "}).mount({ name = 'vf', filetype = 'vf',\n" +
    "  float = { width = 24, height = 4, border = 'rounded', title = 'win float', grab = true } })"));
  await sleep(250);
  const wf = await page.evaluate(() => {
    const chrome = document.querySelector("#grid .float-win");
    const out = { exists: !!chrome };
    if (chrome) {
      out.bg = getComputedStyle(chrome).backgroundColor;
      const rows = [...chrome.querySelectorAll(".row")].map((r) => r.textContent);
      out.top = rows[0]; out.bottom = rows[rows.length - 1];
      out.title = chrome.querySelector(".float-title")?.textContent ?? null;
      out.chromeHasStatus = !!chrome.querySelector(".statusline");
    }
    const content = [...document.querySelectorAll("#grid .win")].find((x) => /alpha/.test(x.textContent));
    out.contentFirstRow = content ? content.querySelector(".row")?.textContent : null;
    out.contentHasStatus = content ? !!content.querySelector(".statusline") : "no-content";
    return out;
  });
  check("window float: opaque chrome box (no bleed-through)",
    wf.exists && wf.bg !== "rgba(0, 0, 0, 0)" && wf.bg !== "transparent", JSON.stringify(wf.bg));
  check("window float: rounded border with title on the top edge",
    !!wf.top && wf.top.startsWith("╭") && wf.top.endsWith("╮") && /win float/.test(wf.title || ""),
    JSON.stringify({ top: wf.top, title: wf.title }));
  check("window float: bottom edge rounded", !!wf.bottom && wf.bottom.startsWith("╰") && wf.bottom.endsWith("╯"),
    JSON.stringify(wf.bottom));
  check("window float: content rendered inside the ring (shows the buffer)",
    /alpha/.test(wf.contentFirstRow || ""), JSON.stringify(wf.contentFirstRow));
  check("window float: NO statusline (float carries none)",
    wf.chromeHasStatus === false && wf.contentHasStatus === false, JSON.stringify(wf));

  await browser.close();
} finally {
  cleanup();
}

process.exit(failures ? 1 : 0);
